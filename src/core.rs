use futures::unsync::mpsc::UnboundedSender;
use futures::Sink;
use shell_words::ParseError;
use std::any::{Any, TypeId};
use std::cell::{RefCell, RefMut, Ref};
use std::collections::HashMap;
use std::fmt;
use std::hash;
use std::io::Error as IoError;
use std::rc::Rc;
use std::string::FromUtf8Error;
use tokio_xmpp::Packet;
use uuid::Uuid;
use xmpp_parsers::{Element, FullJid, BareJid, Jid};
use xmpp_parsers;

#[derive(Debug, Clone)]
pub struct XmppMessage {
    pub id: String,
    pub from: BareJid,
    pub from_full: Jid,
    pub to: BareJid,
    pub to_full: Jid,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct LogMessage {
    pub id: String,
    pub body: String,
}

#[derive(Debug, Clone)]
pub enum Message {
    Incoming(XmppMessage),
    Outgoing(XmppMessage),
    Log(LogMessage),
}

impl Message {
    pub fn incoming<I: Into<String>>(id: I, from_full: &Jid, to_full: &Jid, body: &str) -> Self {
        let from = match from_full {
            Jid::Bare(from_full) => from_full.clone(),
            Jid::Full(from_full) => from_full.clone().into(),
        };

        let to = match to_full {
            Jid::Bare(to_full) => to_full.clone(),
            Jid::Full(to_full) => to_full.clone().into(),
        };

        Message::Incoming(XmppMessage {
            id: id.into(),
            from: from,
            from_full: from_full.clone(),
            to: to.clone(),
            to_full: to_full.clone(),
            body: body.to_string(),
        })
    }

    pub fn outgoing<I: Into<String>>(id: I, from_full: &Jid, to_full: &Jid, body: &str) -> Self {
        let from = match from_full {
            Jid::Bare(from_full) => from_full.clone(),
            Jid::Full(from_full) => from_full.clone().into(),
        };

        let to = match to_full {
            Jid::Bare(to_full) => to_full.clone(),
            Jid::Full(to_full) => to_full.clone().into(),
        };

        Message::Outgoing(XmppMessage {
            id: id.into(),
            from: from,
            from_full: from_full.clone(),
            to: to.clone(),
            to_full: to_full.clone(),
            body: body.to_string(),
        })
    }

    pub fn log(msg: String) -> Self {
        Message::Log(LogMessage {
            id: Uuid::new_v4().to_string(),
            body: msg
        })
    }

    #[allow(dead_code)]
    pub fn body(&self) -> &str {
        match self {
            Message::Outgoing(XmppMessage { body, .. })
                | Message::Incoming(XmppMessage { body, .. })
                | Message::Log(LogMessage { body, .. }) => &body,
        }
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Message::Log(message) => write!(f, "{}", message.body),
            Message::Incoming(message) | Message::Outgoing(message) => write!(f, "{}: {}", message.from, message.body),
        }
    }
}

impl hash::Hash for Message {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        match self {
            Message::Log(message) => message.id.hash(state),
            Message::Incoming(message) | Message::Outgoing(message) => message.id.hash(state),
        }
    }
}

pub enum CommandOrMessage {
    Command(Command),
    Message(Message),
}

#[derive(Debug, Clone)]
pub struct Command {
    pub name: String,
    pub args: Vec<String>,
}

impl Command {
    pub fn new(command: String, args: Vec<String>) -> Self {
        Self {
            name: command,
            args: args,
        }
    }
}

#[derive(Debug, Error)]
pub enum CommandError {
    Io(IoError),
    Utf8(FromUtf8Error),
    Parse(ParseError),
}

pub trait Plugin: fmt::Display {
    fn new() -> Self where Self: Sized;
    fn init(&mut self, mgr: &Aparte) -> Result<(), ()>;
    fn on_connect(&mut self, aparte: Rc<Aparte>);
    fn on_disconnect(&mut self, aparte: Rc<Aparte>);
    fn on_message(&mut self, aparte: Rc<Aparte>, message: &mut Message);
}

pub trait AnyPlugin: Any + Plugin {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn as_plugin(&mut self) -> &mut dyn Plugin;
}

impl<T> AnyPlugin for T where T: Any + Plugin {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn as_plugin(&mut self) -> &mut dyn Plugin {
        self
    }
}

pub struct Connection {
    sink: UnboundedSender<Packet>,
    account: FullJid,
}

pub struct Aparte {
    commands: HashMap<String, fn(Rc<Aparte>, &Command) -> Result<(), ()>>,
    plugins: HashMap<TypeId, RefCell<Box<dyn AnyPlugin>>>,
    connections: RefCell<HashMap<String, Connection>>,
}

impl Aparte {
    pub fn new() -> Self {
        Self {
            commands: HashMap::new(),
            plugins: HashMap::new(),
            connections: RefCell::new(HashMap::new()),
        }
    }

    pub fn add_command(&mut self, name: &str, command: fn(Rc<Aparte>, &Command) -> Result<(), ()>) {
        self.commands.insert(name.to_string(), command);
    }

    pub fn parse_command(self: Rc<Self>, command: &Command) -> Result<(), ()> {
        match self.commands.get(&command.name) {
            Some(parser) => parser(self, command),
            None => Err(()),
        }
    }

    pub fn add_plugin<T: 'static>(&mut self, plugin: Box<dyn AnyPlugin>) -> Result<(), ()> {
        info!("Add plugin `{}`", plugin);
        self.plugins.insert(TypeId::of::<T>(), RefCell::new(plugin));
        Ok(())
    }

    pub fn get_plugin<T: 'static>(&self) -> Option<Ref<T>> {
        let rc = match self.plugins.get(&TypeId::of::<T>()) {
            Some(rc) => rc,
            None => return None,
        };

        let any_plugin = rc.borrow();
        /* Calling unwrap here on purpose as we expect panic if plugin is not of the right type */
        Some(Ref::map(any_plugin, |p| p.as_any().downcast_ref::<T>().unwrap()))
    }

    pub fn get_plugin_mut<T: 'static>(&self) -> Option<RefMut<T>> {
        let rc = match self.plugins.get(&TypeId::of::<T>()) {
            Some(rc) => rc,
            None => return None,
        };

        let any_plugin = rc.borrow_mut();
        /* Calling unwrap here on purpose as we expect panic if plugin is not of the right type */
        Some(RefMut::map(any_plugin, |p| p.as_any_mut().downcast_mut::<T>().unwrap()))
    }

    pub fn add_connection(&self, account: FullJid, sink: UnboundedSender<Packet>) {
        let connection = Connection {
            account: account,
            sink: sink,
        };

        self.connections.borrow_mut().insert(connection.account.to_string(), connection);
    }

    pub fn init(&mut self) -> Result<(), ()> {
        for (_, plugin) in self.plugins.iter() {
            if let Err(err) = plugin.borrow_mut().as_plugin().init(&self) {
                return Err(err);
            }
        }

        Ok(())
    }

    pub fn send(&self, element: Element) {
        trace!("SEND: {:?}", element);
        let packet = Packet::Stanza(element);
        // TODO use correct connection
        let mut connections = self.connections.borrow_mut();
        let current_connection = connections.iter_mut().next().unwrap().1;
        let mut sink = &current_connection.sink;
        if let Err(e) = sink.start_send(packet) {
            warn!("Cannot send packet: {}", e);
        }
    }

    pub fn on_connect(self: Rc<Self>) {
        for (_, plugin) in self.plugins.iter() {
            plugin.borrow_mut().as_plugin().on_connect(Rc::clone(&self));
        }
    }

    pub fn on_message(self: Rc<Self>, message: &mut Message) {
        for (_, plugin) in self.plugins.iter() {
            plugin.borrow_mut().as_plugin().on_message(Rc::clone(&self), message);
        }
    }

    pub fn log(self: Rc<Self>, message: String) {
        let mut message = Message::log(message);
        self.on_message(&mut message);
    }
}
