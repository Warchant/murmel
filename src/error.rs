//
// Copyright 2018 Tamas Blummer
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
//!
//! # SPV Error
//!
//! All modules of this library use this error class to indicate problems.
//!


use rusqlite;
use std::convert;
use std::error::Error;
use std::fmt;
use std::io;
use std::net::SocketAddr;

/// An error class to offer a unified error interface upstream
pub enum SPVError {
    /// There is no peer in node's collection with this socket address
    UnknownPeer (SocketAddr),
    /// generic error message
    Generic(String),
    /// The peer is not following the Bitcoin protocol, the first parameter is added to its ban score
    Misbehaving(u16, String, SocketAddr),
    /// Network IO error
    IO(io::Error),
    /// Database error
    DB(rusqlite::Error),
    /// fatal error, should panic
    Panic(String),
}

impl Error for SPVError {
    fn description(&self) -> &str {
        match *self {
            SPVError::UnknownPeer(_) => "unkown peer",
            SPVError::Generic(ref s) => s.as_str(),
            SPVError::Misbehaving(_, ref reason, _) => reason.as_str(),
            SPVError::IO(ref err) => err.description(),
            SPVError::Panic(ref reason) => reason.as_str(),
            SPVError::DB(ref err) => err.description()
        }
    }

    fn cause(&self) -> Option<&Error> {
        match *self {
            SPVError::UnknownPeer(_) => None,
            SPVError::Generic(_) => None,
            SPVError::IO(ref err) => Some(err),
            SPVError::Misbehaving(_, _, _) => None,
            SPVError::Panic(_) => None,
            SPVError::DB(ref err) => Some(err)
        }
    }
}

impl fmt::Display for SPVError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            // Both underlying errors already impl `Display`, so we defer to
            // their implementations.
            SPVError::UnknownPeer(ref a) => write!(f, "Unknown peer={}", a),
            SPVError::Generic(ref s) => write!(f, "Generic: {}", s),
            SPVError::IO(ref err) => write!(f, "IO error: {}", err),
            SPVError::Misbehaving(_, ref reason, ref peer) => write!(f, "Misbehaving: {} peer={}", reason, peer),
            SPVError::Panic(ref reason) => write!(f, "Panic: {}", reason),
            SPVError::DB(ref err) => write!(f, "DB error: {}", err),
        }
    }
}

impl fmt::Debug for SPVError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        (self as &fmt::Display).fmt(f)
    }
}

impl convert::From<SPVError> for io::Error {
    fn from(err: SPVError) -> io::Error {
        match err {
            SPVError::IO(e) => e,
            _ => io::Error::new(io::ErrorKind::Other, err.description())
        }
    }
}

impl convert::From<io::Error> for SPVError {
    fn from(err: io::Error) -> SPVError {
        SPVError::IO(err)
    }
}

impl convert::From<rusqlite::Error> for SPVError {
    fn from(err: rusqlite::Error) -> SPVError {
        SPVError::DB(err)
    }
}


