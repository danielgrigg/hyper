use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::thread;
use std::vec;

use ::futures::{Future, Poll};
use ::futures_cpupool::{CpuPool, CpuFuture};

pub struct Dns {
    pool: CpuPool,
}

impl Dns {
    pub fn new(threads: usize) -> Dns {
        Dns {
            pool: CpuPool::new(threads)
        }
    }

    pub fn resolve<S: Into<String>>(&self, hostname: S) -> Query {
        let hostname = hostname.into();
        Query(self.pool.spawn_fn(move || work(hostname)))
    }
}

pub struct Query(CpuFuture<IpAddrs, io::Error>);

impl Future for Query {
    type Item = Answer;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.0.poll()
    }
}

pub struct IpAddrs {
    iter: vec::IntoIter<SocketAddr>,
}

impl Iterator for IpAddrs {
    type Item = IpAddr;
    #[inline]
    fn next(&mut self) -> Option<IpAddr> {
        self.iter.next().map(|addr| addr.ip())
    }
}

pub type Answer = io::Result<IpAddrs>;

fn work(hostname: String) -> Answer {
    debug!("resolve {:?}", hostname);
    (&*hostname, 80).to_socket_addrs().map(|i| IpAddrs { iter: i })
}
/*

fn work(rx: spmc::Receiver<String>, notify: channel::Sender<Answer>) {
    thread::Builder::new().name(String::from("hyper-dns")).spawn(move || {
        let mut worker = Worker::new(rx, notify);
        let rx = worker.rx.as_ref().expect("Worker lost rx");
        let notify = worker.notify.as_ref().expect("Worker lost notify");
        while let Ok(host) = rx.recv() {
            debug!("resolve {:?}", host);
            let res = match (&*host, 80).to_socket_addrs().map(|i| IpAddrs{ iter: i }) {
                Ok(addrs) => (host, Ok(addrs)),
                Err(e) => (host, Err(e))
            };

            if let Err(_) = notify.send(res) {
                break;
            }
        }
        worker.shutdown = true;
    }).expect("spawn dns thread");
}

struct Worker {
    rx: Option<spmc::Receiver<String>>,
    notify: Option<channel::Sender<Answer>>,
    shutdown: bool,
}

impl Worker {
    fn new(rx: spmc::Receiver<String>, notify: channel::Sender<Answer>) -> Worker {
        Worker {
            rx: Some(rx),
            notify: Some(notify),
            shutdown: false,
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if !self.shutdown {
            trace!("Worker.drop panicked, restarting");
            work(self.rx.take().expect("Worker lost rx"),
                self.notify.take().expect("Worker lost notify"));
        } else {
            trace!("Worker.drop shutdown, closing");
        }
    }
}
*/
