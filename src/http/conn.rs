use std::borrow::Cow;
use std::fmt;
use std::hash::Hash;
use std::io::{self, Write};
use std::marker::PhantomData;
use std::mem;
use std::time::Duration;

use futures::{Poll, Async};
use tokio::io::{Io, FramedIo};
use tokio_proto::pipeline::Frame;

use http::{self, h1, Http1Transaction, IoBuf, WriteBuf};
use http::h1::{Encoder, Decoder};
use http::buffer::Buffer;
use version::HttpVersion;


/// This handles a connection, which will have been established over a
/// Transport (like a socket), and will likely include multiple
/// `Transaction`s over HTTP.
///
/// The connection will determine when a message begins and ends, creating
/// a new message `TransactionHandler` for each one, as well as determine if this
/// connection can be kept alive after the message, or if it is complete.
pub struct Conn<I, T> {
    io: IoBuf<I>,
    keep_alive_enabled: bool,
    state: State,
    _marker: PhantomData<T>
}

impl<I, T> Conn<I, T> {
    pub fn new(transport: I) -> Conn<I, T> {
        Conn {
            io: IoBuf {
                read_buf: Buffer::new(),
                write_buf: Buffer::new(),
                transport: transport,
            },
            keep_alive_enabled: true,
            state: State::Init,
            _marker: PhantomData,
        }
    }
}

impl<I: Io, T: Http1Transaction> Conn<I, T> {

    fn parse(&mut self) -> ::Result<Option<http::MessageHead<T::Incoming>>> {
        self.io.parse::<T>()
    }

    /*
    fn tick(&mut self) -> Poll<(), ::error::Void> {
        loop {
            let next_state;
            match self.state {
                State::Init { .. } => {
                    trace!("State::Init tick");
                    let (version, head) = match self.parse() {
                        Ok(Some(head)) => (head.version, Ok(head)),
                        Ok(None) => return Ok(Async::NotReady),
                        Err(e) => {
                            self.io.buf.consume_leading_lines();
                            if !self.io.buf.is_empty() {
                                trace!("parse error ({}) with bytes: {:?}", e, self.io.buf.bytes());
                                (HttpVersion::Http10, Err(e))
                            } else {
                                trace!("parse error with 0 input, err = {:?}", e);
                                self.state = State::Closed;
                                return Ok(Async::Ready(()));
                            }
                        }
                    };

                    match version {
                        HttpVersion::Http10 | HttpVersion::Http11 => {
                            let handler = match self.handler.transaction() {
                                Some(h) => h,
                                None => {
                                    trace!("could not create txn handler, key={:?}", self.key);
                                    self.state = State::Closed;
                                    return Ok(Async::Ready(()));
                                }
                            };
                            let res = head.and_then(|head| {
                                let decoder = <<H as ConnectionHandler<T>>::Txn as TransactionHandler<T>>::Transaction::decoder(&head);
                                decoder.map(move |decoder| (head, decoder))
                            });
                            next_state = State::Http1(h1::Txn::incoming(res, handler));
                        },
                        _ => {
                            warn!("unimplemented HTTP Version = {:?}", version);
                            self.state = State::Closed;
                            return Ok(Async::Ready(()));
                        }
                    }

                },
                State::Http1(ref mut http1) => {
                    trace!("Stte::Http1 tick");
                    match http1.tick(&mut self.io) {
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Ok(Async::Ready(TxnResult::KeepAlive)) => {
                            trace!("Http1 Txn tick complete, keep-alive");
                            //TODO: check if keep-alive is enabled
                            next_state = State::Init {};
                        },
                        Ok(Async::Ready(TxnResult::Close)) => {
                            trace!("Http1 Txn tick complete, close");
                            next_state = State::Closed;
                        },
                        Err(void) => match void {}
                    }
                },
                //State::Http2
                State::Closed => {
                    trace!("State::Closed tick");
                    return Ok(Async::Ready(()));
                }
            }

            self.state = next_state;
        }
    }
    */
}

impl<I, T> FramedIo for Conn<I, T>
where I: Io,
      T: Http1Transaction,
      T::Outgoing: fmt::Debug {
    type In = Frame<http::MessageHead<T::Outgoing>, http::Chunk, ::Error>;
    type Out = Frame<http::MessageHead<T::Incoming>, http::Chunk, ::Error>;

    fn poll_read(&mut self) -> Async<()> {
        let ret = match self.state {
            State::Init => self.io.transport.poll_read(),
            State::Closed => Async::Ready(()),
            _ => Async::NotReady
        };
        trace!("Conn::poll_read = {:?}", ret);
        ret
    }

    fn read(&mut self) -> Poll<Self::Out, io::Error> {
        trace!("Conn::read");
        match self.state {
            State::Init => {
                trace!("Conn::read State::Init");
                // parse on a brand new connection
                let (version, head) = match self.parse() {
                    Ok(Some(head)) => (head.version, head),
                    Ok(None) => return Ok(Async::NotReady),
                    Err(e) => {
                        self.io.read_buf.consume_leading_lines();
                        if !self.io.read_buf.is_empty() {
                            error!("parse error ({}) with bytes: {:?}", e, self.io.read_buf.bytes());
                            self.state = State::Closed;
                            return Ok(Async::Ready(Frame::Error { error: e }));
                        } else {
                            trace!("parse error with 0 input, err = {:?}", e);
                            return Ok(Async::Ready(Frame::Done));
                        }
                    }
                };

                match version {
                    HttpVersion::Http10 | HttpVersion::Http11 => {
                        let decoder = match T::decoder(&head) {
                            Ok(d) => d,
                            Err(e) => {
                                error!("decoder error = {:?}", e);
                                self.state = State::Closed;
                                return Ok(Async::Ready(Frame::Error { error: e }));
                            }
                        };
                        let (body, reading) = if decoder.is_eof() {
                            (false, Reading::KeepAlive)
                        } else {
                            (true, Reading::Body(decoder))
                        };
                        self.state = State::Http1 {
                            reading: reading,
                            writing: Writing::Init,
                        };
                        return Ok(Async::Ready(Frame::Message { message: head, body: body }));
                    },
                    _ => {
                        error!("unimplemented HTTP Version = {:?}", version);
                        self.state = State::Closed;
                        return Ok(Async::Ready(Frame::Error { error: ::Error::Version }));
                    }
                }
            },
            State::Http1 { .. } => {
                trace!("Conn::read State::Http1");
                return Ok(Async::NotReady);
            },
            State::Closed => {
                trace!("Conn::read State::Closed");
                return Ok(Async::Ready(Frame::Done));
            }
        }
    }

    fn poll_write(&mut self) -> Async<()> {
        trace!("Conn::poll_write");
        //self.io.transport.poll_write()
        Async::Ready(())
    }

    fn write(&mut self, frame: Self::In) -> Poll<(), io::Error> {
        trace!("Conn::write");
        match self.state {
            State::Init => {
                unimplemented!("Conn::write State::Init");
            },
            State::Http1 { .. } => {
                match frame {
                    Frame::Message { message: mut head, body } => {
                        trace!("Conn::write Frame::Message with_body = {:?}", body);
                        let mut buf = Vec::new();
                        T::encode(&mut head, &mut buf);
                        self.io.write(&buf).unwrap();
                    },
                    Frame::Body { chunk: Some(body) } => {
                        trace!("Conn::write Http1 Frame::Body = Some");
                        self.io.write(&body).unwrap();
                    },
                    Frame::Body { chunk: None } => {
                        trace!("Conn::write Http1 Frame::Body = None");
                    }
                    Frame::Error { error } => {
                        error!("Conn::write Frame::Error err = {:?}", error);
                        self.state = State::Closed;
                    },
                    Frame::Done => {
                        trace!("Conn::write Frame::Done");
                        self.state = State::Closed;
                    }
                }
                return Ok(Async::Ready(()));
            }
            State::Closed => {
                error!("Conn::write Closed frame = {:?}", frame);
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "State::Closed write"));
            }
        }
    }

    fn flush(&mut self) -> Poll<(), io::Error> {
        trace!("Conn::flush");
        match self.io.flush() {
            Ok(()) => {
                if self.poll_read().is_ready() {
                    ::futures::task::park().unpark();
                }
                Ok(Async::Ready(()))
            },
            Err(e) => match e.kind() {
                io::ErrorKind::WouldBlock => Ok(Async::NotReady),
                _ => Err(e)
            }
        }
    }
}

impl<I, T> fmt::Debug for Conn<I, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Conn")
            .field("keep_alive_enabled", &self.keep_alive_enabled)
            .field("state", &self.state)
            .field("io", &self.io)
            .finish()
    }
}

impl<I, T> Drop for Conn<I, T> {
    fn drop(&mut self) {
        trace!("Conn::drop");
    }
}

#[derive(Debug)]
enum State {
    Init,
    Http1 {
        reading: Reading,
        writing: Writing,
    },
    Closed,
}

#[derive(Debug)]
enum Reading {
    Init,
    Body(Decoder),
    KeepAlive,
    Closed,
}

#[derive(Debug)]
enum Writing {
    Init,
    Body(Encoder),
    KeepAlive,
    Closed,
}

#[cfg(test)]
mod tests {
    use futures::Async;
    use tokio::io::FramedIo;
    use tokio_proto::pipeline::Frame;

    use http::{MessageHead, ServerTransaction};
    use mock::AsyncIo;
    
    use super::Conn;

    #[test]
    fn test_conn_init_read() {
        let good_message = b"GET / HTTP/1.1\r\n\r\n".to_vec();
        let len = good_message.len();
        let io = AsyncIo::new_buf(good_message, len);
        let mut conn = Conn::<_, ServerTransaction>::new(io);

        match conn.read().unwrap() {
            Async::Ready(Frame::Message { message, body: false }) => {
                assert_eq!(message, MessageHead {
                    subject: ::http::RequestLine(::Get, ::RequestUri::AbsolutePath {
                        path: "/".to_string(),
                        query: None,
                    }),
                    .. MessageHead::default()
                })
            },
            f => panic!("frame is not a message {:?}", f)
        }
    }
}
