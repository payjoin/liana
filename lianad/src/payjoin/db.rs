use payjoin::bitcoin::{Psbt, Txid};
use payjoin::persist::SessionPersister;
use payjoin::receive::v2::ReceiverSessionEvent;
use payjoin::send::v2::SenderSessionEvent;
use serde::{Deserialize, Serialize};
use std::fmt::{self, Display, Formatter};
use std::sync::Arc;
use std::time::SystemTime;

use crate::database::DatabaseInterface;

use super::types::PayjoinStatus;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub status: PayjoinStatus,
    pub maybe_txid: Option<Txid>,
    pub maybe_psbt: Option<Psbt>,
    pub maybe_bip21: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionWrapper<V> {
    pub metadata: SessionMetadata,
    pub events: Vec<V>,
    pub completed_at: Option<SystemTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionId(pub u64);

impl SessionId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
}

#[derive(Debug)]
pub enum PersisterError {
    Serialize(serde_json::Error),
    Deserialize(serde_json::Error),
    NotFound(String),
}

impl Display for PersisterError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            PersisterError::Serialize(e) => write!(f, "Serialization failed: {e}"),
            PersisterError::Deserialize(e) => write!(f, "Deserialization failed: {e}"),
            PersisterError::NotFound(key) => write!(f, "Key not found: {key}"),
        }
    }
}

impl std::error::Error for PersisterError {}

#[derive(Clone)]
pub struct ReceiverPersister {
    db: Arc<dyn DatabaseInterface>,
    pub session_id: SessionId,
}

impl ReceiverPersister {
    pub fn new(db: Arc<dyn DatabaseInterface>) -> Result<Self, ()> {
        let mut db_conn = db.connection();
        let session_id = SessionId::new(db_conn.payjoin_next_id("payjoin_receivers"));
        let session: SessionWrapper<ReceiverSessionEvent> = SessionWrapper {
            metadata: SessionMetadata {
                status: PayjoinStatus::Pending,
                maybe_txid: None,
                maybe_psbt: None,
                maybe_bip21: None,
            },
            events: vec![],
            completed_at: None,
        };
        db_conn.payjoin_save_receiver_session(&session_id, session);
        Ok(Self { db, session_id })
    }

    pub fn from_id(db: Arc<dyn DatabaseInterface>, id: SessionId) -> Result<Self, ()> {
        Ok(Self { db, session_id: id })
    }

    pub fn update_metada(
        &self,
        status: Option<PayjoinStatus>,
        maybe_txid: Option<Txid>,
        maybe_psbt: Option<Psbt>,
        maybe_bip21: Option<String>,
    ) {
        let mut db_conn = self.db.connection();
        if let Some(mut session) = db_conn.payjoin_get_receiver_session(&self.session_id) {
            session.metadata = SessionMetadata {
                status: status.unwrap_or(PayjoinStatus::Pending),
                maybe_txid,
                maybe_psbt,
                maybe_bip21,
            };
            db_conn.update_payjoin_receiver_status(&self.session_id, session);
        }
    }
}

impl SessionPersister for ReceiverPersister {
    type SessionEvent = ReceiverSessionEvent;
    type InternalStorageError = PersisterError;

    fn save_event(
        &self,
        event: &Self::SessionEvent,
    ) -> std::result::Result<(), Self::InternalStorageError> {
        let mut db_conn = self.db.connection();
        // Check if key exists
        if let Some(mut session) = db_conn.payjoin_get_receiver_session(&self.session_id) {
            // Append new event
            session.events.push(event.clone());
            db_conn.update_payjoin_receiver_status(&self.session_id, session);
        }
        Ok(())
    }

    fn load(
        &self,
    ) -> std::result::Result<Box<dyn Iterator<Item = Self::SessionEvent>>, Self::InternalStorageError>
    {
        let mut db_conn = self.db.connection();
        let session = db_conn
            .payjoin_get_receiver_session(&self.session_id)
            .expect("key should exist");
        Ok(Box::new(session.events.into_iter()))
    }

    fn close(&self) -> std::result::Result<(), Self::InternalStorageError> {
        let mut db_conn = self.db.connection();
        if let Some(mut session) = db_conn.payjoin_get_receiver_session(&self.session_id) {
            session.completed_at = Some(SystemTime::now());
            db_conn.update_payjoin_receiver_status(&self.session_id, session);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct SenderPersister {
    db: Arc<dyn DatabaseInterface>,
    pub session_id: SessionId,
}

impl SenderPersister {
    pub fn new(db: Arc<dyn DatabaseInterface>) -> Result<Self, ()> {
        let mut db_conn = db.connection();
        let session_id = SessionId::new(db_conn.payjoin_next_id("payjoin_senders"));
        let session: SessionWrapper<SenderSessionEvent> = SessionWrapper {
            metadata: SessionMetadata {
                status: PayjoinStatus::Pending,
                maybe_txid: None,
                maybe_psbt: None,
                maybe_bip21: None,
            },
            events: vec![],
            completed_at: None,
        };
        db_conn.payjoin_save_sender_session(&session_id, session);
        Ok(Self { db, session_id })
    }

    pub fn from_id(db: Arc<dyn DatabaseInterface>, id: SessionId) -> Result<Self, ()> {
        Ok(Self { db, session_id: id })
    }

    pub fn update_metada(
        &self,
        status: Option<PayjoinStatus>,
        maybe_txid: Option<Txid>,
        maybe_psbt: Option<Psbt>,
        maybe_bip21: Option<String>,
    ) {
        let mut db_conn = self.db.connection();
        if let Some(mut session) = db_conn.payjoin_get_sender_session(&self.session_id) {
            session.metadata = SessionMetadata {
                status: status.unwrap_or(PayjoinStatus::Pending),
                maybe_txid,
                maybe_psbt,
                maybe_bip21,
            };
            db_conn.update_payjoin_sender_status(&self.session_id, session);
        }
    }
}

impl SessionPersister for SenderPersister {
    type SessionEvent = SenderSessionEvent;
    type InternalStorageError = PersisterError;

    fn save_event(
        &self,
        event: &Self::SessionEvent,
    ) -> std::result::Result<(), Self::InternalStorageError> {
        let mut db_conn = self.db.connection();
        // Check if key exists
        if let Some(mut session) = db_conn.payjoin_get_sender_session(&self.session_id) {
            // Append new event
            session.events.push(event.clone());
            db_conn.update_payjoin_sender_status(&self.session_id, session);
        }
        Ok(())
    }

    fn load(
        &self,
    ) -> std::result::Result<Box<dyn Iterator<Item = Self::SessionEvent>>, Self::InternalStorageError>
    {
        let mut db_conn = self.db.connection();
        let session = db_conn
            .payjoin_get_sender_session(&self.session_id)
            .expect("key should exist");
        Ok(Box::new(session.events.into_iter()))
    }

    fn close(&self) -> std::result::Result<(), Self::InternalStorageError> {
        let mut db_conn = self.db.connection();
        if let Some(mut session) = db_conn.payjoin_get_sender_session(&self.session_id) {
            session.completed_at = Some(SystemTime::now());
            db_conn.update_payjoin_sender_status(&self.session_id, session);
        }
        Ok(())
    }
}
