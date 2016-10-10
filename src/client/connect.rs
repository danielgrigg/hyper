use std::collections::hash_map::{HashMap, Entry};
use std::hash::Hash;
use std::fmt;
use std::io;
use std::net::SocketAddr;

use futures::{Future, Poll, Async};
use tokio::reactor::Handle;
use tokio::tcp::{TcpStream, TcpStreamNew};
use url::Url;

use net::{Transport, HttpStream, /*HttpsStream, SslClient*/};
use super::dns;
use super::Registration;

pub type DefaultConnector = HttpConnector;
#[doc(hidden)]
pub type DefaultTransport = <DefaultConnector as Connect>::Output;


/// A connector creates a Transport to a remote address..
pub trait Connect {
    /// Type of Transport to create
    type Output: Transport;
    /// dox
    type Fut: Future<Item=Self::Output, Error=io::Error>;
    /// The key used to determine if an existing socket can be used.
    type Key: Eq + Hash + Clone + fmt::Debug;
    /// Returns the key based off the Url.
    fn key(&self, &Url) -> Option<Self::Key>;
    /// Connect to a remote address.
    fn connect(&mut self, &Url) -> Self::Fut;
}

type Scheme = String;
type Port = u16;

/// A connector for the `http` scheme.
pub struct HttpConnector {
    dns: Option<Dns>,
    threads: usize,
    //resolving: HashMap<String, Vec<(&'static str, String, u16)>>,
}

impl HttpConnector {
    /// Set the number of resolver threads.
    ///
    /// Default is 4.
    pub fn threads(mut self, threads: usize) -> HttpConnector {
        debug_assert!(self.dns.is_none(), "setting threads after Dns is created does nothing");
        self.threads = threads;
        self
    }
}

impl Default for HttpConnector {
    fn default() -> HttpConnector {
        HttpConnector {
            dns: None,
            threads: 4,
            resolving: HashMap::new(),
        }
    }
}

impl fmt::Debug for HttpConnector {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("HttpConnector")
            .field("threads", &self.threads)
            .finish()
    }
}

impl Connect for HttpConnector {
    type Output = HttpStream;
    type Fut = Connecting;
    type Key = (&'static str, String, u16);

    fn dns_workers(&mut self, count: usize) {
        self.threads = count;
    }

    fn key(&self, url: &Url) -> Option<Self::Key> {
        if url.scheme() == "http" {
            Some((
                "http",
                url.host_str().expect("http scheme must have host").to_owned(),
                url.port().unwrap_or(80),
            ))
        } else {
            None
        }
    }

    fn connect(&mut self, url: &Url) -> Self::Fut {
        debug!("Http::connect({:?})", url);
        if let Some(key) = self.key(url) {
            let host = url.host_str().expect("http scheme must have a host");

            Connecting {
                state: State::Resolving(self.dns.as_ref().expect("dns workers lost").query(host))
            }
        } else {
            Err(io::Error::new(io::ErrorKind::InvalidInput, "scheme must be http"))
        }
    }

    fn connected(&mut self) -> Option<(Self::Key, io::Result<HttpStream>)> {
        let (host, addrs) = match self.dns.as_ref().expect("dns workers lost").resolved() {
            Ok(res) => res,
            Err(_) => return None
        };
        //TODO: try all addrs
        let addr = addrs.and_then(|mut addrs| Ok(addrs.next().unwrap()));
        debug!("Http::resolved <- ({:?}, {:?})", host, addr);
        if let Entry::Occupied(mut entry) = self.resolving.entry(host) {
            let resolved = entry.get_mut().remove(0);
            if entry.get().is_empty() {
                entry.remove();
            }
            let port = resolved.2;
            Some((resolved, addr.and_then(|addr| TcpStream::connect(&SocketAddr::new(addr, port))
                                                            .map(HttpStream))
                ))
        } else {
            trace!("^--  resolved but not in hashmap?");
            None
        }
    }

    fn register(&mut self, reg: Registration) {
        self.dns = Some(Dns::new(reg.notify, self.threads));
    }
}

pub struct Connecting {
    state: State,
    handle: Handle,
}

enum State {
    Resolving(dns::Query),
    Connecting(ConnectingTcp),
    Error(Option<io::Error>)
}

impl Future for Connecting {
    type Item = HttpStream;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let state;
            match self.state {
                State::Resolving(ref mut query) => {
                    match try!(query.poll()) {
                        Async::NotReady => return Ok(Async::NotReady),
                        Async::Ready(addrs) => {
                            state = State::Connecting(ConnectingTcp {
                                addrs: addrs,
                                current: None,
                            })
                        }
                    }
                },
                State::Connecting(ref mut c) => c.poll(&self.handle),
                State::Error(err) => return err.take().expect("polled Connecting too many times"),
            }
            self.state = state;
        }
    }
}

struct ConnectingTcp {
    addrs: dns::IpAddrs,
    current: Option<TcpStreamNew>,
}

impl ConnectingTcp {
    // not a Future, since passing a &Handle to poll
    fn poll(&mut self, handle: &Handle) -> Poll<TcpStream, io::Error> {
        let mut err = None;
        loop {
            if let Some(ref mut current) = self.current {
                match current.poll() {
                    Ok(ok) => return Ok(ok),
                    Err(e) => {
                        trace!("connect error {:?}", e);
                        err = Some(e);
                        if let Some(addr) = self.addrs.next() {
                            *current = TcpStream::connect(&addr, handle);
                            continue;
                        }
                    }
                }
            } else if let Some(addr) = self.addrs.next() {
                self.current = Some(sock);
                continue;
            }

            return Err(err.take().expect("missing connect error"));
        }
    }
}

/*
/// A connector that can protect HTTP streams using SSL.
#[derive(Debug, Default)]
pub struct HttpsConnector<S: SslClient> {
    http: HttpConnector,
    ssl: S
}

impl<S: SslClient> HttpsConnector<S> {
    /// Create a new connector using the provided SSL implementation.
    pub fn new(s: S) -> HttpsConnector<S> {
        HttpsConnector {
            http: HttpConnector::default(),
            ssl: s,
        }
    }
}

impl<S: SslClient> Connect for HttpsConnector<S> {
    type Output = HttpsStream<S::Stream>;
    type Key = (&'static str, String, u16);

    fn dns_workers(&mut self, count: usize) {
        self.http.dns_workers(count)
    }

    fn key(&self, url: &Url) -> Option<Self::Key> {
        let scheme = match url.scheme() {
            "http" => "http",
            "https" => "https",
            _ => return None
        };
        Some((
            scheme,
            url.host_str().expect("http scheme must have host").to_owned(),
            url.port_or_known_default().expect("http scheme must have a port"),
        ))
    }

    fn connect(&mut self, url: &Url) -> io::Result<Self::Key> {
        debug!("Https::connect({:?})", url);
        if let Some(key) = self.key(url) {
            let host = url.host_str().expect("http scheme must have a host");
            self.http.dns.as_ref().expect("dns workers lost").resolve(host);
            self.http.resolving.entry(host.to_owned()).or_insert_with(Vec::new).push(key.clone());
            Ok(key)
        } else {
            Err(io::Error::new(io::ErrorKind::InvalidInput, "scheme must be http or https"))
        }
    }

    fn connected(&mut self) -> Option<(Self::Key, io::Result<Self::Output>)> {
        self.http.connected().map(|(key, res)| {
            let res = res.and_then(|http| {
                if key.0 == "https" {
                    self.ssl.wrap_client(http, &key.1)
                        .map(HttpsStream::Https)
                        .map_err(|e| match e {
                            ::Error::Io(e) => e,
                            e => io::Error::new(io::ErrorKind::Other, e)
                        })
                } else {
                    Ok(HttpsStream::Http(http))
                }
            });
            (key, res)
        })
    }

    fn register(&mut self, reg: Registration) {
        self.http.register(reg);
    }
}

#[cfg(not(any(feature = "openssl", feature = "security-framework")))]
#[doc(hidden)]
pub type DefaultConnector = HttpConnector;

#[cfg(all(feature = "openssl", not(feature = "security-framework")))]
#[doc(hidden)]
pub type DefaultConnector = HttpsConnector<::net::Openssl>;

#[cfg(feature = "security-framework")]
#[doc(hidden)]
pub type DefaultConnector = HttpsConnector<::net::SecureTransportClient>;

#[doc(hidden)]
pub type DefaultTransport = <DefaultConnector as Connect>::Output;

fn _assert_defaults() {
    fn _assert<T, U>() where T: Connect<Output=U>, U: Transport {}

    _assert::<DefaultConnector, DefaultTransport>();
}
*/
