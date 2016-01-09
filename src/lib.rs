#![feature(const_fn)]
#![feature(custom_derive, plugin)]
#![plugin(serde_macros)]

extern crate serde;
extern crate serde_json;

use serde::Deserialize;
use std::fmt;
use std::io::{self, Read};
use std::convert;
use std::collections::HashMap;
use std::net::{
    self,
    TcpListener,
    TcpStream,
};
use std::sync::{
    self,
    Mutex,
    Arc,
};
use std::sync::mpsc::{
    channel,
    sync_channel,
    Sender,
    SyncSender,
    Receiver,
};
use std::time;
use std::thread;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Json(serde_json::Error),
    Sender,
    Unimplemented,
    Impossible
}

impl convert::From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Error {
        match err {
            serde_json::Error::IoError(err) => Error::Io(err),
            err => Error::Json(err),
        }
    }
}

impl convert::From<io::Error> for Error {
    fn from(err: io::Error) -> Error {
        Error::Io(err)
    }
}

impl<T> convert::From<sync::mpsc::SendError<T>> for Error {
    fn from(_: sync::mpsc::SendError<T>) -> Error {
        Error::Sender
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn handle_conn<F, Request, Reply>(mut stream: TcpStream, f: Arc<F>) -> Result<()>
    where Request: fmt::Debug + serde::de::Deserialize,
          Reply: fmt::Debug + serde::ser::Serialize,
          F: Serve<Request, Reply>
{
    let read_stream = try!(stream.try_clone());
    let mut de = serde_json::Deserializer::new(read_stream.bytes());
    loop {
        println!("read");
        let request_packet: Packet<Request> = try!(Packet::deserialize(&mut de));
        match request_packet {
            Packet::Shutdown => break,
            Packet::Message(id, message) => {
                let reply = try!(f.serve(&message));
                let reply_packet = Packet::Message(id, reply);
                println!("write");
                try!(serde_json::to_writer(&mut stream, &reply_packet));
            },
        }
    }
    Ok(())
}

pub fn serve<F, Request, Reply>(listener: TcpListener, f: Arc<F>) -> Error
    where Request: fmt::Debug + serde::de::Deserialize,
          Reply: fmt::Debug + serde::ser::Serialize,
          F: 'static + Serve<Request, Reply>,
{
    for conn in listener.incoming() {
        let conn = match conn {
            Err(err) => return convert::From::from(err),
            Ok(c) => c,
        };
        let f = f.clone();
        thread::spawn(move || {
            if let Err(err) = handle_conn(conn, f) {
                println!("error handling connection: {:?}", err);
            }
        });
    }
    Error::Impossible
}

pub trait Serve<Request, Reply>: Send + Sync {
    fn serve(&self, request: &Request) -> io::Result<Reply>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum Packet<T> {
    Message(u64, T),
    Shutdown,
}

struct Handle<T> {
    id: u64,
    sender: Sender<T>,
}

enum ReceiverMessage<Reply> {
    Handle(Handle<Reply>),
    Packet(Packet<Reply>),
    Shutdown,
}

fn receiver<Reply>(messages: Receiver<ReceiverMessage<Reply>>) -> Result<()> {
    let mut ready_handles: HashMap<u64, Handle<Reply>> = HashMap::new();
    for message in messages.into_iter() {
        match message {
            ReceiverMessage::Handle(handle) => {
                ready_handles.insert(handle.id, handle);
            },
            ReceiverMessage::Packet(Packet::Shutdown) => break,
            ReceiverMessage::Packet(Packet::Message(id, message)) => {
                let handle = ready_handles.remove(&id).unwrap();
                try!(handle.sender.send(message));
            }
            ReceiverMessage::Shutdown => break,
        }
    }
    Ok(())
}

fn reader<Reply>(stream: TcpStream, tx: SyncSender<ReceiverMessage<Reply>>)
    where Reply: serde::Deserialize
{
    use serde_json::Error::SyntaxError;
    use serde_json::ErrorCode::EOFWhileParsingValue;
    let mut de = serde_json::Deserializer::new(stream.bytes());
    loop {
        match Packet::deserialize(&mut de) {
            Ok(packet) =>{
                println!("send!");
                tx.send(ReceiverMessage::Packet(packet)).unwrap();
            },
            // TODO: This shutdown logic is janky.. What's the right way to do this?
            Err(SyntaxError(EOFWhileParsingValue, _, _)) => break,
            Err(err) => panic!("unexpected error while parsing!: {:?}", err),
        }
    }
}

fn increment(cur_id: &mut u64) -> u64 {
    let id = *cur_id;
    *cur_id += 1;
    id
}

struct SyncedClientState<Reply> {
    next_id: u64,
    stream: TcpStream,
    handles_tx: SyncSender<ReceiverMessage<Reply>>,
}

pub struct Client<Reply> {
    synced_state: Mutex<SyncedClientState<Reply>>,
    reader_guard: thread::JoinHandle<()>,
}

impl<Reply> Client<Reply>
    where Reply: serde::de::Deserialize + Send + 'static
{
    pub fn new(stream: TcpStream) -> Result<Self> {
        let (handles_tx, receiver_rx) = sync_channel(0);
        let read_stream = try!(stream.try_clone());
        try!(read_stream.set_read_timeout(Some(time::Duration::from_millis(50))));
        let reader_handles_tx = handles_tx.clone();
        let guard = thread::spawn(move || reader(read_stream, reader_handles_tx));
        thread::spawn(move || receiver(receiver_rx));
        Ok(Client{
            synced_state: Mutex::new(SyncedClientState{
                next_id: 0,
                stream: stream,
                handles_tx: handles_tx,
            }),
            reader_guard: guard,
        })
    }

    pub fn rpc<Request>(&self, request: &Request) -> Result<Reply>
        where Request: serde::ser::Serialize + Clone + Send + 'static
    {
        let (tx, rx) = channel();
        let mut state = self.synced_state.lock().unwrap();
        let id = increment(&mut state.next_id);
        try!(state.handles_tx.send(ReceiverMessage::Handle(Handle{
            id: id,
            sender: tx,
        })));
        let packet = Packet::Message(id, request.clone());
        try!(serde_json::to_writer(&mut state.stream, &packet));
        Ok(rx.recv().unwrap())
    }

    pub fn join<Request: serde::Serialize>(self) -> Result<()> {
        let mut state = self.synced_state.lock().unwrap();
        let packet: Packet<Request> = Packet::Shutdown;
        try!(serde_json::to_writer(&mut state.stream, &packet));
        try!(state.stream.shutdown(net::Shutdown::Both));
        self.reader_guard.join().unwrap();
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::io;
    use std::net::{TcpStream, TcpListener, SocketAddr};
    use std::str::FromStr;
    use std::sync::{Arc, Mutex, Barrier};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    const port: AtomicUsize = AtomicUsize::new(10000);

    fn pair() -> (TcpStream, TcpListener) {
        let addr = format!("127.0.0.1:{}", port.fetch_add(1, Ordering::SeqCst));
        println!("what the fuck {}", &addr);
        // Do this one first so that we don't get connection refused :)
        let listener = TcpListener::bind(&*addr).unwrap();
        (TcpStream::connect(&*addr).unwrap(), listener)
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
    enum Request {
        Increment
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    enum Reply {
        Increment(u64)
    }

    struct Server {
        counter: Mutex<u64>,
    }

    impl Serve<Request, Reply> for Server {
        fn serve(&self, _: &Request) -> io::Result<Reply> {
            let mut counter = self.counter.lock().unwrap();
            let reply = Reply::Increment(*counter);
            *counter += 1;
            Ok(reply)
        }
    }

    impl Server {
        fn new() -> Server {
            Server{counter: Mutex::new(0)}
        }

        fn count(&self) -> u64 {
            *self.counter.lock().unwrap()
        }
    }

    #[test]
    fn test() {
        let (client_stream, server_streams) = pair();
        let server = Arc::new(Server::new());
        let thread_server = server.clone();
        let guard = thread::spawn(move || serve(server_streams, thread_server));
        let client = Client::new(client_stream).unwrap();
        assert_eq!(Reply::Increment(0), client.rpc(&Request::Increment).unwrap());
        assert_eq!(1, server.count());
        assert_eq!(Reply::Increment(1), client.rpc(&Request::Increment).unwrap());
        assert_eq!(2, server.count());
        client.join::<Request>().unwrap();
        guard.join();
    }

    struct BarrierServer {
        barrier: Barrier,
        inner: Server,
    }

    impl Serve<Request, Reply> for BarrierServer {
        fn serve(&self, request: &Request) -> io::Result<Reply> {
            self.barrier.wait();
            let reply = try!(self.inner.serve(request));
            Ok(reply)
        }
    }

    impl BarrierServer {
        fn new(n: usize) -> BarrierServer {
            BarrierServer{barrier: Barrier::new(n), inner: Server::new()}
        }

        fn count(&self) -> u64 {
            self.inner.count()
        }
    }

    #[test]
    fn test_concurrent() {
        let (client_stream, server_streams) = pair();
        let server = Arc::new(BarrierServer::new(10));
        let thread_server = server.clone();
        let guard = thread::spawn(move || serve(server_streams, thread_server));
        let client: Arc<Client<Reply>> = Arc::new(Client::new(client_stream).unwrap());
        let mut join_handles = vec![];
        for _ in 0..10 {
            let my_client = client.clone();
            join_handles.push(thread::spawn(move || my_client.rpc(&Request::Increment).unwrap()));
        }
        for handle in join_handles.into_iter() {
            handle.join();
        }
        assert_eq!(10, server.count());
        let client = match Arc::try_unwrap(client) {
            Err(_) => panic!("couldn't unwrap arc"),
            Ok(c) => c,
        };
        client.join::<Request>().unwrap();
        guard.join();
    }
}
