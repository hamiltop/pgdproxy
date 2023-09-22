/*
This file is part of the Rust PostgreSQL Native TLS Connector project.

TODO: LICENSE and Copyright notice
 */

use bytes::{Buf, BytesMut};
use postgres_protocol::message::backend;
use std::io;
use tokio_util::codec::{Decoder, Encoder};

pub trait FrameInfo {
    fn done(&self) -> bool;
    fn command(&self) -> Option<u8>;
}
// Used to forward data from client to postgres
#[derive(Debug)]
pub struct ForwardingClientCodec;

pub type ClientCommand = BytesMut;

impl FrameInfo for ClientCommand {
    // ClientCommands are always done
    fn done(&self) -> bool {
        self[0] != b'P'
    }
    fn command(&self) -> Option<u8> {
        Some(self[0])
    }
}

// Used to forward data from postgres to client
#[derive(Debug)]
pub struct ForwardingBackendCodec;

#[derive(Debug)]
pub struct ForwardingBackendData {
    buf: BytesMut,
    // If request is complete, we can go back to Listening state
    request_complete: bool,
}

impl FrameInfo for ForwardingBackendData {
    fn done(&self) -> bool {
        self.request_complete
    }
    fn command(&self) -> Option<u8> {
        backend::Header::parse(&self.buf).unwrap().map(|h| h.tag())
    }
}

impl Encoder<ForwardingBackendData> for ForwardingClientCodec {
    type Error = io::Error;

    fn encode(&mut self, item: ForwardingBackendData, dst: &mut BytesMut) -> io::Result<()> {
        dst.extend_from_slice(&item.buf);
        Ok(())
    }
}

impl Decoder for ForwardingClientCodec {
    type Item = ClientCommand;

    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() == 0 {
            return Ok(None);
        }
        let _tag = src[0];
        if src.len() < 5 {
            return Ok(None);
        }
        let len = (&src[1..5]).get_i32() as usize;
        if src.len() < len + 1 {
            Ok(None)
        } else {
            let buf = src.split_to(len + 1);
            Ok(Some(buf))
        }
    }
}

impl Encoder<ClientCommand> for ForwardingBackendCodec {
    type Error = io::Error;

    fn encode(&mut self, item: ClientCommand, dst: &mut BytesMut) -> io::Result<()> {
        dst.extend_from_slice(&item);
        Ok(())
    }
}

impl Decoder for ForwardingBackendCodec {
    type Item = ForwardingBackendData;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, io::Error> {
        let mut request_complete = false;

        if let Some(header) = backend::Header::parse(&src)? {
            let len = header.len() as usize + 1;
            if header.tag() == backend::READY_FOR_QUERY_TAG {
                request_complete = true;
            }
            if src.len() < len {
                Ok(None)
            } else {
                Ok(Some(ForwardingBackendData {
                    buf: src.split_to(len),
                    request_complete,
                }))
            }
        } else {
            Ok(None)
        }
    }
}

pub struct StartupRequest;

pub enum SslOrStartup {
    SslRequest([u8; 8]),
    StartupRequest(BytesMut),
}

impl Decoder for StartupRequest {
    type Item = SslOrStartup;

    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // Both SSL and Startup are minimum 8 bytes
        // SSL is len(u32) + code(u32)
        // Startup is len(u32) + version(u32) + rest
        if src.len() < 8 {
            return Ok(None);
        }
        let len = (&src[0..4]).get_i32() as usize;
        if src.len() < len {
            return Ok(None);
        }

        // Check if this is an SSL request
        // 0x04D2162F
        if &src[0..8] == [0, 0, 0, 8, 0x04, 0xD2, 0x16, 0x2F] {
            let mut buf = src.split_to(8);
            let mut dst = [0u8; 8];
            buf.copy_to_slice(&mut dst);
            Ok(Some(SslOrStartup::SslRequest(dst)))
        } else {
            let buf = src.split_to(len);
            Ok(Some(SslOrStartup::StartupRequest(buf)))
        }
    }
}
