use anyhow::{anyhow, Result};
use bytes::Buf;
use futures_util::StreamExt;
use std::{cmp::min, io::Cursor};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    select,
};
use tracing::debug;

use tokio_util::codec::{Decoder, Framed};

pub type TLSStream = Framed<TcpStream, TLSCodec>;

pub struct TLSCodec {
    buf: Vec<u8>,
    cursor: usize,
    pub enable_codec: bool,
}

impl TLSCodec {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(0x2000),
            enable_codec: true,
            cursor: 0,
        }
    }

    pub fn reset(&mut self) {
        assert!(self.cursor == self.buf.len());
        unsafe {
            self.buf.set_len(0);
            self.cursor = 0;
        }
    }

    fn peek_record_length(&self) -> usize {
        5 + ((self.buf[self.cursor + 3] as usize) << 8 | self.buf[self.cursor + 4] as usize)
    }

    pub fn next_record(&mut self) -> &mut [u8] {
        let start = self.cursor;
        self.cursor += self.peek_record_length();
        &mut self.buf[start..self.cursor]
    }

    pub fn peek_record(&self) -> &[u8] {
        let len = self.peek_record_length();
        &self.buf[self.cursor..self.cursor + len]
    }
    pub fn peek_record_type(&self) -> u8 {
        self.buf[self.cursor]
    }

    pub fn has_next(&self) -> bool {
        self.cursor < self.buf.len()
    }

    pub fn skip_to_end(&mut self) {
        self.cursor = self.buf.len();
    }

    pub fn raw_buf(&self) -> &[u8] {
        assert!(self.cursor == self.buf.len());
        &self.buf
    }

    pub fn has_content(&self) -> bool {
        !self.buf.is_empty()
    }
}

impl Decoder for TLSCodec {
    type Item = ();

    type Error = anyhow::Error;

    fn decode(
        &mut self,
        src: &mut bytes::BytesMut,
    ) -> std::result::Result<Option<Self::Item>, Self::Error> {
        self.reset();

        if !self.enable_codec {
            if src.len() == 0 {
                return Ok(None);
            }
            self.buf.extend_from_slice(&src);
            src.advance(src.len());
            return Ok(Some(()));
        }

        if src.len() < 5 {
            debug!("src len < 5");
            return Ok(None);
        }
        let mut cursor = 0;
        while cursor + 5 < src.len() {
            let record_len = ((src[cursor + 3] as u16) << 8 | (src[cursor + 4] as u16)) as usize;
            debug!("incoming record len: {}", record_len);
            if src.len() < cursor + 5 + record_len {
                break;
            }
            cursor += 5 + record_len;
        }
        if cursor == 0 {
            return Ok(None);
        }
        self.buf.reserve(cursor);
        unsafe {
            self.buf.set_len(cursor);
        }

        src.copy_to_slice(&mut self.buf);

        tracing::debug!("decoded: {}", self.buf.len());

        Ok(Some(()))
    }
}

pub(crate) fn read_length_padded_header<const N: usize, T: Buf>(buf: &mut T) -> usize {
    let mut len = 0;
    let mut tmp = [0; 8];
    buf.copy_to_slice(&mut tmp[..N]);
    for i in 0..N {
        len = (len << 8) | (tmp[i] as usize);
    }
    len
}

pub(crate) fn skip_length_padded<const N: usize, T: Buf>(buf: &mut T) -> usize {
    let len = read_length_padded_header::<N, T>(buf);
    buf.advance(len);
    len
}

pub(crate) fn read_length_padded<const N: usize, T: Buf>(buf: &mut T, copy_to: &mut [u8]) -> usize {
    let len = read_length_padded_header::<N, T>(buf);
    assert!(copy_to.len() >= len);
    buf.copy_to_slice(&mut copy_to[..len]);
    len
}

pub(crate) fn extend_from_length_prefixed<const N: usize, T: Buf>(
    buf: &mut T,
    copy_to: &mut Vec<u8>,
) {
    let len = read_length_padded_header::<N, T>(buf);

    copy_to.extend_from_slice(&buf.chunk()[..len]);
    buf.advance(len);
}

pub(crate) fn length_prefixed<const N: usize, T: Buf, P: FnOnce(Cursor<&[u8]>)>(
    buf: &mut T,
    parse: P,
) {
    let len = read_length_padded_header::<N, _>(buf);
    parse(Cursor::new(&buf.chunk()[..len]));
    buf.advance(len);
}

pub(crate) fn u8_length_prefixed<T: Buf, P: FnOnce(Cursor<&[u8]>)>(buf: &mut T, parse: P) {
    length_prefixed::<1, _, _>(buf, parse);
}

pub(crate) fn u16_length_prefixed<T: Buf, P: FnOnce(Cursor<&[u8]>)>(buf: &mut T, parse: P) {
    length_prefixed::<2, _, _>(buf, parse);
}

pub(crate) fn xor_bytes(secret: &[u8], msg: &mut [u8]) {
    for i in 0..min(secret.len(), msg.len()) {
        msg[i] = msg[i] ^ secret[i];
    }
}

pub async fn copy_bidirectional(
    mut inbound: TLSStream,
    mut outbound: TcpStream,
    mut content_offset: usize,
) -> Result<()> {
    let mut out_buf = [0; 0x2000];
    out_buf[..3].copy_from_slice(&[0x17, 0x03, 0x03]);
    while inbound.codec().has_next() {
        outbound
            .write_all(&inbound.codec_mut().next_record()[content_offset..])
            .await?;
        content_offset = 5;
    }

    inbound.codec_mut().reset();

    loop {
        select! {
            res = inbound.next() => {
                match res {
                    Some(Ok(_)) => (),
                    e => {
                        e.ok_or(anyhow!("failed to read from inbound: "))??;
                    }
                }
                while inbound.codec().has_next() {
                    outbound
                        .write_all(&inbound.codec_mut().next_record()[5..])
                        .await?;
                }
                inbound.codec_mut().reset();
            }
            n = outbound.read(&mut out_buf[5..]) => {
                let n = n?;
                if n == 0 {
                    return Err(anyhow!("failed to read from outbound: "));
                }
                out_buf[3..5].copy_from_slice(&(n as u16).to_be_bytes());
                inbound.get_mut().write_all(&out_buf[..n+5]).await?;
            }
        }
    }
}

pub async fn copy_bidirectional_fallback(
    mut inbound: TLSStream,
    mut outbound: TLSStream,
) -> Result<()> {
    inbound.codec_mut().enable_codec = false;
    outbound.codec_mut().enable_codec = false;
    if inbound.codec().has_content() {
        inbound.codec_mut().skip_to_end();
        debug!(
            "write old msg to inbound {}",
            inbound.codec().raw_buf().len()
        );
        outbound
            .get_mut()
            .write_all(inbound.codec().raw_buf())
            .await?;
    }
    if outbound.codec().has_content() {
        outbound.codec_mut().skip_to_end();
        debug!(
            "write old msg to outbound {}",
            outbound.codec().raw_buf().len()
        );
        inbound
            .get_mut()
            .write_all(outbound.codec().raw_buf())
            .await?;
    }

    debug!("start relaying");

    loop {
        select! {
            res = inbound.next() => {
                match res {
                    Some(Ok(_)) => (),
                    e => {
                        e.ok_or(anyhow!("failed to read from inbound: "))??;
                    }
                }
                inbound.codec_mut().skip_to_end();
                outbound.get_mut().write_all(inbound.codec().raw_buf()).await?;
            }
            res = outbound.next() => {
                match res {
                    Some(Ok(_))  => (),
                    e => {
                        e.ok_or(anyhow!("failed to read from outbound: "))??;
                    }
                }
                outbound.codec_mut().skip_to_end();
                inbound.get_mut().write_all(outbound.codec().raw_buf()).await?;
            }
        }
    }
}
