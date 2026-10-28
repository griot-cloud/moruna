//! The schema format (SDD 23, d.2): a protocol is declared once, with [`protocol!`],
//! and the declaration is the protocol. It generates, in a module of the protocol's
//! name: the protocol's [`ProtocolId`](crate::ProtocolId) (`ID`), its typed `Error`
//! and `Event` enums, a `Server` trait with one method per request, `Serve`, the
//! [`Handler`](crate::Handler) that dispatches requests to a `Server`, and `Client`,
//! the typed proxy. Compile-time checks refuse a bad name, an ordinal that is zero or
//! repeated, an error code outside 1 to 65535 or repeated, and a request or event that
//! claims a minor version above the protocol's.
//!
//! ```ignore
//! moruna_svc::protocol! {
//!     /// Echoes bytes back.
//!     pub protocol echo {
//!         name = "moruna.example.echo";
//!         major = 1;
//!         minor = 1;
//!         requests {
//!             /// The bytes, back.
//!             1 => fn echo(text: Vec<u8>) -> Vec<u8>;
//!             /// Writes the bytes to `to`; their length, back. Since 1.1.
//!             2 since 1 => fn send_to(to: Channel, text: Vec<u8>) -> u32;
//!         }
//!         events {
//!             1 => echoed { count: u64 };
//!         }
//!         errors {
//!             1 => Empty;
//!         }
//!     }
//! }
//! ```
//!
//! A field's type is anything that implements [`Wire`](crate::Wire): integers, `bool`,
//! `()`, `String`, `Vec<T>`, `Option<T>`, tuples, a `Handle` or a `Channel`.

/// Declares a protocol; see the [module](self) documentation.
#[macro_export]
macro_rules! protocol {
    (
        $(#[$meta:meta])*
        $vis:vis protocol $module:ident {
            name = $name:literal;
            major = $major:literal;
            minor = $minor:literal;
            requests {
                $(
                    $(#[$rmeta:meta])*
                    $rord:literal $(since $rsince:literal)? => fn $method:ident ( $( $arg:ident : $aty:ty ),* $(,)? ) -> $rty:ty ;
                )*
            }
            events {
                $(
                    $(#[$emeta:meta])*
                    $eord:literal $(since $esince:literal)? => $event:ident { $( $field:ident : $fty:ty ),* $(,)? } ;
                )*
            }
            errors {
                $(
                    $(#[$xmeta:meta])*
                    $code:literal => $error:ident ;
                )*
            }
        }
    ) => {
        $(#[$meta])*
        #[allow(non_camel_case_types, unreachable_code, clippy::too_many_arguments)]
        $vis mod $module {
            #[allow(unused_imports)]
            use super::*;

            /// The protocol's name and version.
            pub const ID: $crate::ProtocolId = $crate::ProtocolId::new($name, $major, $minor);
            /// The ordinals of the protocol's requests.
            pub const REQUESTS: &[u16] = &[$($rord),*];
            /// The ordinals of the protocol's events.
            pub const EVENTS: &[u16] = &[$($eord),*];

            const _: () = assert!(
                $crate::protocol::ordinals_valid(REQUESTS),
                "request ordinals must be nonzero and distinct"
            );
            const _: () = assert!(
                $crate::protocol::ordinals_valid(EVENTS),
                "event ordinals must be nonzero and distinct"
            );
            const _: () = assert!(
                $crate::protocol::codes_valid(&[$($code),*]),
                "error codes must be 1 to 65535 and distinct"
            );
            $( const _: () = assert!(
                $crate::protocol::since_valid($crate::__since!($($rsince)?), $minor),
                "a request cannot arrive in a minor version above the protocol's"
            ); )*
            $( const _: () = assert!(
                $crate::protocol::since_valid($crate::__since!($($esince)?), $minor),
                "an event cannot arrive in a minor version above the protocol's"
            ); )*

            /// The protocol's errors.
            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            pub enum Error {
                $( $(#[$xmeta])* $error, )*
            }

            impl $crate::ProtocolError for Error {
                fn code(&self) -> u32 {
                    match *self {
                        $( Error::$error => $code, )*
                    }
                }
                fn from_code(code: u32) -> ::core::option::Option<Self> {
                    match code {
                        $( $code => ::core::option::Option::Some(Error::$error), )*
                        _ => ::core::option::Option::None,
                    }
                }
            }

            /// The protocol's events.
            #[derive(Debug)]
            pub enum Event {
                $( $(#[$emeta])* $event { $( $field: $fty ),* }, )*
            }

            impl Event {
                /// The event's ordinal.
                pub fn ordinal(&self) -> u16 {
                    match *self {
                        $( Event::$event { .. } => $eord, )*
                    }
                }

                /// The minor version the event arrived in.
                pub fn since(&self) -> u16 {
                    match *self {
                        $( Event::$event { .. } => $crate::__since!($($esince)?), )*
                    }
                }

                /// Decodes event `ordinal` from `d`.
                pub fn decode(
                    ordinal: u16,
                    d: &mut $crate::Decoder<'_>,
                ) -> ::core::result::Result<Event, $crate::WireError> {
                    let event = match ordinal {
                        $( $eord => Event::$event { $( $field: <$fty as $crate::Wire>::decode(d)? ),* }, )*
                        _ => return ::core::result::Result::Err($crate::WireError::Invalid),
                    };
                    d.finish()?;
                    ::core::result::Result::Ok(event)
                }

                /// Encodes the event's fields.
                pub fn encode(self, e: &mut $crate::Encoder) -> ::core::result::Result<(), $crate::WireError> {
                    match self {
                        $( Event::$event { $( $field ),* } => {
                            $( $crate::Wire::encode($field, e)?; )*
                        } )*
                    }
                    ::core::result::Result::Ok(())
                }

                /// Sends the event on the connection a request arrived on; an event
                /// newer than the client's minor version is not sent (`Unsupported`).
                pub fn emit(self, cx: &mut $crate::Context<'_>) -> ::core::result::Result<(), $crate::Transport> {
                    let (ordinal, since) = (self.ordinal(), self.since());
                    let mut e = $crate::Encoder::new();
                    self.encode(&mut e).map_err(|_| $crate::Transport::TooLarge)?;
                    cx.emit(ordinal, since, e)
                }
            }

            /// What a server of the protocol implements: one method per request.
            pub trait Server {
                $(
                    $(#[$rmeta])*
                    fn $method(
                        &mut self,
                        cx: &mut $crate::Context<'_>,
                        $( $arg: $aty ),*
                    ) -> ::core::result::Result<$rty, Error>;
                )*
            }

            /// The [`Handler`]($crate::Handler) that serves a [`Server`] on a connection.
            pub struct Serve<S>(pub S);

            impl<S: Server> $crate::Handler for Serve<S> {
                fn protocol(&self) -> $crate::ProtocolId {
                    ID
                }

                #[allow(unused_variables)]
                fn handle(
                    &mut self,
                    cx: &mut $crate::Context<'_>,
                    ordinal: u16,
                    d: &mut $crate::Decoder<'_>,
                ) -> $crate::Outcome {
                    match ordinal {
                        $( $rord => {
                            $(
                                let ::core::result::Result::Ok($arg) = <$aty as $crate::Wire>::decode(d) else {
                                    return $crate::Outcome::Status($crate::Status::BadMessage);
                                };
                            )*
                            if d.finish().is_err() {
                                return $crate::Outcome::Status($crate::Status::BadMessage);
                            }
                            match self.0.$method(cx, $( $arg ),*) {
                                ::core::result::Result::Ok(reply) => $crate::Outcome::reply(reply),
                                ::core::result::Result::Err(e) => {
                                    $crate::Outcome::Failure($crate::ProtocolError::code(&e))
                                }
                            }
                        } )*
                        _ => $crate::Outcome::Status($crate::Status::UnknownMethod),
                    }
                }
            }

            /// Makes a listener's factory: each new connection gets a fresh server.
            pub fn factory<S: Server + 'static>(
                mut make: impl FnMut() -> S + 'static,
            ) -> $crate::Factory {
                $crate::__private::Box::new(move || {
                    $crate::__private::Box::new(Serve(make())) as $crate::__private::Box<dyn $crate::Handler>
                })
            }

            /// The typed proxy: one method per request, each a call that waits for its
            /// reply within the client's timeout.
            pub struct Client {
                core: $crate::ClientCore,
            }

            impl Client {
                /// Says `Hello` on `channel` and waits for the server's answer until
                /// `deadline`.
                pub fn connect(
                    channel: $crate::Channel,
                    deadline: u64,
                ) -> ::core::result::Result<Client, $crate::CallError<Error>> {
                    $crate::ClientCore::connect(channel, ID, deadline)
                        .map(|core| Client { core })
                        .map_err($crate::CallError::typed)
                }

                /// The connection underneath: the server's version, the timeout, the
                /// channel.
                pub fn core(&mut self) -> &mut $crate::ClientCore {
                    &mut self.core
                }

                /// The next event, waiting until `deadline`.
                pub fn next_event(&mut self, deadline: u64) -> ::core::result::Result<Event, $crate::CallError<Error>> {
                    let mut frame = self.core.next_event(deadline).map_err($crate::CallError::typed)?;
                    let ordinal = frame.ordinal;
                    let mut d = frame.decoder();
                    Event::decode(ordinal, &mut d).map_err(|_| $crate::CallError::Malformed)
                }

                $(
                    $(#[$rmeta])*
                    pub fn $method(
                        &mut self,
                        $( $arg: $aty ),*
                    ) -> ::core::result::Result<$rty, $crate::CallError<Error>> {
                        if !$crate::protocol::since_valid($crate::__since!($($rsince)?), self.core.server_minor()) {
                            return ::core::result::Result::Err($crate::CallError::Unsupported);
                        }
                        #[allow(unused_mut)]
                        let mut e = $crate::Encoder::new();
                        $( $crate::Wire::encode($arg, &mut e).map_err($crate::CallError::from)?; )*
                        let mut frame = self.core.call($rord, e).map_err($crate::CallError::typed)?;
                        let mut d = frame.decoder();
                        let reply = <$rty as $crate::Wire>::decode(&mut d).map_err(|_| $crate::CallError::Malformed)?;
                        d.finish().map_err(|_| $crate::CallError::Malformed)?;
                        ::core::result::Result::Ok(reply)
                    }
                )*
            }
        }
    };
}

/// The minor version a request or event arrived in: its `since`, or 0.
#[doc(hidden)]
#[macro_export]
macro_rules! __since {
    () => {
        0u16
    };
    ($since:literal) => {
        $since
    };
}
