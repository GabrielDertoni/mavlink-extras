use std::marker::PhantomData;

use bytes::{Buf, BufMut};
use crc_any::CRCu16;
use mavlink::{
    common::MavMessage,
    error::{MessageReadError, MessageWriteError},
    MavHeader, MavFrame, MavlinkVersion, Message,
};
use tokio_util::codec::{Decoder, Encoder};

use crate::MaybeRawMsg;

pub struct MavMessageSerde<M>(PhantomData<fn(M) -> M>);

impl<M: mavlink::Message> MavMessageSerde<M> {
    pub fn new() -> MavMessageSerde<M> {
        MavMessageSerde(PhantomData)
    }
}

// MAVLink v1 protocol message layout:
//
//     0 ... 1 ... 2 ... 3 ... 4 .... 5 ... 6 ...... n+6 ...... n+8
//     +-----+-----+-----+-----+------+-----+---------+----------+
//     | STX | LEN | SEQ | SYS | COMP | MSG | PAYLOAD | CHECKSUM |
//     |     | (n) |     | ID  |  ID  | ID  |         |          |
//     +-----+-----+-----+-----+------+-----+---------+----------+
//
//
// MAVLink v2 protocol message layout:
//
//     0 ... 1 ... 2 ..... 3 ..... 4 ... 5 ... 6 .... 7 .. 10 ...... n+10 ..... n+12 ....... n+25
//     +-----+-----+-------+-------+-----+-----+------+-----+---------+----------+------------+
//     | STX | LEN |  INC  |  CMP  | SEQ | SYS | COMP | MSG | PAYLOAD | CHECKSUM | SIGNATURE  |
//     |     | (n) | FLAGS | FLAGS |     | ID  |  ID  | ID  |         |          | (optional) |
//     +-----+-----+-------+-------+-----+-----+------+-----+---------+----------+------------+
//
//     STX:         1 byte
//     LEN:         1 byte
//     INC FLAGS:   1 byte
//     CMP FLAGS:   1 byte
//     SEQ:         1 byte
//     SYS ID:      1 byte
//     COMP ID:     1 byte
//     MSG ID:      3 bytes
//     PAYLOAD: 0-255 bytes
//     CHECKSUM:    2 bytes
//     SIGNATURE:  13 bytes
//

// source: https://github.com/iridia-ulb/supervisor/blob/34220c6b9627f5e6cc02a190ac3dc9bd849ab6b5/src/robot/drone/codec.rs
impl<M: Message> Decoder for MavMessageSerde<M> {
    type Item = MavFrame<M>;
    type Error = MessageReadError;

    fn decode(&mut self, src: &mut bytes::BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        use mavlink::{MAV_STX, MAV_STX_V2};

        let index = src
            .iter()
            .position(|&b| [MAV_STX_V2, MAV_STX].contains(&b))
            .unwrap_or(src.remaining());

        // discard everything up to but excluding STX
        src.advance(index);

        let Some(&stx) = src.get(0) else { return Ok(None); };
        let Some(payload_len) = src.get(1).map(|&v| v as usize) else { return Ok(None); };
        let (header_len, ver) = match stx {
            // MAVLink v1
            MAV_STX    => (6, MavlinkVersion::V1),
            // MAVLink v2
            MAV_STX_V2 => (10, MavlinkVersion::V2),
            // Corrupted
            _ => return Ok(None),
        };
        if src.remaining() < header_len {
            return Ok(None);
        }
        let mut header_bytes = &src[..header_len];
        let header = &mut header_bytes;
        header.advance(1); // Skip over STX

        // Must start calculating the CRC from now
        let mut crc_calc = CRCu16::crc16mcrf4cc();
        let (tail_len, has_signature) = match ver {
            MavlinkVersion::V1 => {
                crc_calc.digest(header.as_ref());
                header.advance(1); // Skip payload len
                let tail_len = payload_len + 2; // payload + checksum
                (tail_len, false)
            }
            MavlinkVersion::V2 => {
                crc_calc.digest(header.as_ref());
                header.advance(1); // Skip payload len
                let has_signature = header.get_u8() & 1 == 1;
                header.advance(1); // Skip CMP FLAGS
                // Payload + checksum + maybe signature
                let tail_len = payload_len + 2 + if has_signature { 13 } else { 0 };
                (tail_len, has_signature)
            }
        };

        let seq = header.get_u8();
        let sysid = header.get_u8();
        let compid = header.get_u8();
        let msgid = match ver {
            MavlinkVersion::V1 => header.get_u8() as u32,
            MavlinkVersion::V2 => {
                // Header has 3 bytes now, but we need 4 to make up a u32.
                let mut bytes = [0; 4];
                header.copy_to_slice(&mut bytes[..3]);
                u32::from_le_bytes(bytes)
            }
        };

        // Need more data
        if src.remaining() < header_len + tail_len {
            return Ok(None);
        }
        src.advance(header_len);

        let mut payload = src.split_to(payload_len);
        crc_calc.digest(payload.as_ref());
        let crc = src.get_u16_le();
        if has_signature {
            // We're just going to ignore the signature for now.
            // TODO: Use this feature
            src.advance(13);
        }

        crc_calc.digest(&[MavMessage::extra_crc(msgid)]);
        if crc_calc.get_crc() != crc {
            // CRC check failed, skip this message
            /* hack: we should have a CRC error here */
            return Ok(None);
        }

        let msg = M::parse(ver, msgid, payload.as_ref())?;
        payload.advance(payload.remaining());
        Ok(Some(MavFrame {
            protocol_version: ver,
            header: MavHeader {
                sequence:     seq,
                system_id:    sysid,
                component_id: compid,
            },
            msg,
        }))
    }
}

impl<M: Message> Encoder<MavFrame<MaybeRawMsg<M>>> for MavMessageSerde<M> {
    type Error = MessageWriteError;

    fn encode(
        &mut self,
        item: MavFrame<MaybeRawMsg<M>>,
        dst: &mut bytes::BytesMut,
    ) -> Result<(), Self::Error> {
        match item.protocol_version {
            MavlinkVersion::V1 => {
                mavlink::write_v1_msg(&mut dst.writer(), item.header, &item.msg)?;
            }

            MavlinkVersion::V2 => {
                mavlink::write_v2_msg(&mut dst.writer(), item.header, &item.msg)?;
            }
        }
        Ok(())
    }
}
