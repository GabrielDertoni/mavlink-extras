pub mod mavconn;
pub mod mailbox;
pub mod codec;

#[cfg(feature = "serde")]
use serde::{Serialize, Deserialize};

#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct MavMsgRaw {
    pub id:      u32,
    pub len:     u8, // Don't have type info to indicate the size
   #[cfg_attr(feature = "serde", serde(with = "serde_arrays"))]
    pub payload: [u8; 48],
}

impl MavMsgRaw {
    pub fn from_template<M: mavlink::Message>(template: M) -> Self {
        let bytes = template.ser();
        assert!(bytes.len() <= 48);
        let mut payload = [0_u8; 48];
        payload[..bytes.len()].copy_from_slice(&bytes);
        Self {
            id: template.message_id(),
            len: bytes.len() as u8,
            payload,
        }
    }

    pub fn payload(&mut self) -> &mut [u8] {
        &mut self.payload[..self.len as usize]
    }
}

#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum MaybeRawMsg<M> {
    Standard(M),
    // TODO: Improve this.
    Raw(MavMsgRaw),
}

impl<M: mavlink::Message> mavlink::Message for MaybeRawMsg<M> {
    fn message_id(&self) -> u32 {
        match self {
            Self::Standard(msg) => msg.message_id(),
            Self::Raw(msg) => msg.id,
        }
    }

    fn message_name(&self) -> &'static str {
        match self {
            Self::Standard(msg) => msg.message_name(),
            Self::Raw(_) => "unknown",
        }
    }

    fn ser(&self) -> Vec<u8> {
        match self {
            Self::Standard(msg) => msg.ser(),
            Self::Raw(raw) => Vec::from(&raw.payload[..raw.len as usize]),
        }
    }

    fn parse(
        version: mavlink::MavlinkVersion,
        msgid: u32,
        payload: &[u8],
    ) -> Result<Self, mavlink::error::ParserError> {
        Ok(M::parse(version, msgid, payload).map_or_else(
            |_| {
                let mut p = [0_u8; 48];
                p.copy_from_slice(payload);
                MaybeRawMsg::Raw(MavMsgRaw {
                    id:      msgid,
                    len:     payload.len() as u8,
                    payload: p,
                })
            },
            MaybeRawMsg::Standard,
        ))
    }

    fn message_id_from_name(name: &str) -> Result<u32, &'static str> {
        M::message_id_from_name(name)
    }

    fn default_message_from_id(id: u32) -> Result<Self, &'static str> {
        Ok(M::default_message_from_id(id).map_or_else(
            |_| {
                Self::Raw(MavMsgRaw {
                    id,
                    len: 0,
                    payload: [0; 48],
                })
            },
            Self::Standard,
        ))
    }

    fn extra_crc(id: u32) -> u8 {
        M::extra_crc(id)
    }
}

impl<M: mavlink::Message> From<MavMsgRaw> for MaybeRawMsg<M> {
    fn from(msg: MavMsgRaw) -> Self {
        MaybeRawMsg::Raw(msg)
    }
}

impl<M: mavlink::Message> From<M> for MaybeRawMsg<M> {
    fn from(msg: M) -> Self {
        MaybeRawMsg::Standard(msg)
    }
}
