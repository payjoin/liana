use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PayjoinSenderStatus {
    Pending = 0,
    WaitingReceiver = 1,
    Completed = 2,
    // TODO: more specific enums for why it failed
    Failed = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum PayjoinReceiverStatus {
    Pending = 0,
    Signing = 1,
    Completed = 2,
    // TODO: more specific enums for why it failed
    Failed = 3,
}

impl From<i32> for PayjoinSenderStatus {
    fn from(status: i32) -> Self {
        match status {
            0 => PayjoinSenderStatus::Pending,
            1 => PayjoinSenderStatus::WaitingReceiver,
            2 => PayjoinSenderStatus::Completed,
            3 => PayjoinSenderStatus::Failed,
            _ => panic!("Invalid payjoin sender status: {}", status),
        }
    }
}

impl From<i32> for PayjoinReceiverStatus {
    fn from(status: i32) -> Self {
        match status {
            0 => PayjoinReceiverStatus::Pending,
            1 => PayjoinReceiverStatus::Signing,
            2 => PayjoinReceiverStatus::Completed,
            3 => PayjoinReceiverStatus::Failed,
            _ => panic!("Invalid payjoin receiver status: {}", status),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum PayjoinStatus {
    Pending = 0,
    WaitingReceiver = 1,
    Signing = 2,
    Completed = 3,
    // TODO: more specific enums for why it failed
    Failed = 4,
}

impl From<i32> for PayjoinStatus {
    fn from(status: i32) -> Self {
        match status {
            0 => PayjoinStatus::Pending,
            1 => PayjoinStatus::WaitingReceiver,
            2 => PayjoinStatus::Signing,
            3 => PayjoinStatus::Completed,
            4 => PayjoinStatus::Failed,
            _ => panic!("Invalid payjoin status: {}", status),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PayjoinInfo {
    pub status: Option<PayjoinStatus>,
    pub bip21: String,
}
