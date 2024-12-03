use std::{path::PathBuf, pin::Pin, process::id, sync::Arc, time::{Duration, SystemTime, UNIX_EPOCH}};

use log::{debug, error, info, warn};
use plist::{Data, Value};
use serde::{Deserialize, Serialize};
use tokio::{select, sync::{broadcast, Mutex}, task::JoinHandle};
use uuid::Uuid;

use crate::{aps::{get_message, APSConnection}, imessage::messages::{MessageTarget, SendMessage}, util::{bin_deserialize_opt_vec, encode_hex, plist_to_bin, ungzip}, APSMessage, ConversationData, IDSUser, Message, MessageInst, OSConfig, PushError};

use super::{identity_manager::{DeliveryHandle, IdentityManager, IdentityResource}, messages::{ErrorMessage, SUPPORTED_COMMANDS}, user::{IDSUserIdentity, QueryOptions}};
use std::str::FromStr;
use rand::RngCore;
use async_recursion::async_recursion;

#[derive(Deserialize)]
pub struct MadridRecvMessage {
    // all messages
    #[serde(rename = "c")]
    pub command: u8,
    #[serde(rename = "e")]
    pub ns_since_epoch: Option<u64>,

    #[serde(default, rename = "U", deserialize_with = "bin_deserialize_opt_vec")]
    pub uuid: Option<Vec<u8>>,
    #[serde(rename = "sP")]
    pub sender: Option<String>,
    #[serde(default, rename = "t", deserialize_with = "bin_deserialize_opt_vec")]
    pub token: Option<Vec<u8>>,
    #[serde(rename = "tP")]
    pub target: Option<String>,
    #[serde(rename = "nr")]
    pub no_reply: Option<bool>,

    // for c = 100
    #[serde(rename = "eX")]
    pub is_typing: Option<u64>,
    #[serde(rename = "D")]
    pub send_delivered: Option<bool>,

    // old iOS participants change
    #[serde(rename = "p")]
    message_unenc: Option<Value>,

    #[serde(default, rename = "P", deserialize_with = "bin_deserialize_opt_vec")]
    message: Option<Vec<u8>>,

    // for confirm
    #[serde(rename = "s")]
    status: Option<i64>,

    #[serde(default, rename = "fU", deserialize_with = "bin_deserialize_opt_vec")]
    error_for: Option<Vec<u8>>,
    #[serde(rename = "fRM")]
    error_string: Option<String>,
    #[serde(rename = "fR")]
    error_status: Option<u64>,
    #[serde(rename = "fM")]
    error_for_str: Option<String>,

    #[serde(skip)]
    verification_failed: bool,
}

impl MadridRecvMessage {
    pub fn to_message(&self, conversation: Option<ConversationData>, message: Message) -> Result<MessageInst, PushError> {
        let Self {
            sender,
            uuid: Some(uuid),
            ns_since_epoch: Some(ns_since_epoch),
            token,
            send_delivered,
            ..
        } = self else {
            return Err(PushError::BadMsg)
        };
        Ok(MessageInst {
            sender: sender.clone(),
            id: Uuid::from_bytes(uuid.clone().try_into().unwrap()).to_string().to_uppercase(),
            sent_timestamp: ns_since_epoch / 1000000,
            conversation,
            message,
            target: token.clone().map(|token| vec![MessageTarget::Token(token)]),
            send_delivered: send_delivered.unwrap_or(false),
            verification_failed: self.verification_failed,
        })
    }
}

pub struct IMClient {
    pub conn: APSConnection,
    pub identity: IdentityManager,
    raw_inbound: Mutex<broadcast::Receiver<APSMessage>>,
    os_config: Arc<dyn OSConfig>,
}

impl IMClient {
    pub async fn new(conn: APSConnection, users: Vec<IDSUser>, identity: IDSUserIdentity, cache_path: PathBuf, os_config: Arc<dyn OSConfig>, mut keys_updated: Box<dyn FnMut(Vec<IDSUser>) + Send + Sync>) -> IMClient {
        
        let _ = Self::setup_conn(&conn).await;

        let mut to_refresh = conn.generated_signal.subscribe();
        let reconn_conn = Arc::downgrade(&conn);
        tokio::spawn(async move {
            loop {
                match to_refresh.recv().await {
                    Ok(()) => {
                        let Some(conn) = reconn_conn.upgrade() else { break };
                        let _ = Self::setup_conn(&conn).await;
                    },
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        let identity = IdentityResource::new(users, identity, cache_path, conn.clone(), os_config.clone()).await;

        let mut to_refresh = identity.generated_signal.subscribe();
        let my_ident_ref = identity.resource.clone();
        tokio::spawn(async move {
            loop {
                match to_refresh.recv().await {
                    Ok(()) => {
                        keys_updated(my_ident_ref.users.read().await.clone())
                    },
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        IMClient {
            raw_inbound: Mutex::new(conn.messages_cont.subscribe()),
            conn,
            os_config: os_config.clone(),
            identity,
        }
    }

    async fn setup_conn(conn: &APSConnection) -> Result<(), PushError> {
        conn.send(APSMessage::SetState { state: 1 }).await?;
        conn.filter(&["com.apple.private.alloy.sms"], &[], &["com.apple.madrid"], &[]).await?;

        if let Err(_) = tokio::time::timeout(Duration::from_millis(500), conn.wait_for_timeout(conn.subscribe().await,
            |msg| if let APSMessage::NoStorage = msg { Some(()) } else { None })).await {

            debug!("Flushing cache!");
            
            #[derive(Serialize)]
            struct FlushCacheMsg {
                c: u64,
                e: u64,
            }

            let start = SystemTime::now();
            let since_the_epoch = start
                .duration_since(UNIX_EPOCH)
                .expect("Time went backwards");
            let msg = FlushCacheMsg {
                c: 160,
                e: since_the_epoch.as_nanos() as u64,
            };

            conn.send_message("com.apple.madrid", plist_to_bin(&msg).unwrap(), None).await?;
        }
        Ok(())
    }

    pub async fn receive_wait(&self) -> Result<Option<MessageInst>, PushError> {
        let mut filter = get_message(|load| {
            debug!("recv {:?}", load);
            let parsed: MadridRecvMessage = plist::from_value(&load).ok()?;
            if SUPPORTED_COMMANDS.contains(&parsed.command) {
                    Some(parsed)
                } else { None }
        }, &["com.apple.madrid", "com.apple.private.alloy.sms"]);
        loop {
            let msg = self.raw_inbound.lock().await.recv().await.unwrap();
            if let Some(received) = filter(msg) {
                let recieved = self.process_msg(received).await;
                if let Ok(Some(recieved)) = &recieved { info!("recieved {recieved}"); }
                return recieved
            }
        }
    }
    
    async fn process_msg(&self, mut payload: MadridRecvMessage) -> Result<Option<MessageInst>, PushError> {
        let command = payload.command;
        // delivered/read
        if let MadridRecvMessage {
            command: 101 | 102,
            ..
        } = &payload {
            return Ok(payload.to_message(None, if command == 101 {
                Message::Delivered
            } else {
                Message::Read
            }).ok())
        }

        // typing
        if let MadridRecvMessage {
            sender: Some(sender),
            target: Some(target),
            is_typing: Some(0),
            message,
            ..
        } = &payload {
            return Ok(payload.to_message(Some(ConversationData {
                participants: vec![sender.clone(), target.clone()],
                cv_name: None,
                sender_guid: None,
                after_guid: None,
            }), if message.is_some() {
                Message::StopTyping
            } else {
                Message::Typing
            }).ok())
        }

        // errors
        if let MadridRecvMessage {
            command: 120,
            error_for: Some(_),
            error_status: Some(error_status),
            error_string: Some(error_string),
            error_for_str: Some(for_str),
            ..
        } = &payload {
            return Ok(payload.to_message(None, Message::Error(ErrorMessage {
                for_uuid: for_str.clone(),
                status: *error_status,
                status_str: error_string.clone(),
            })).ok())
        }

        // TODO rewrite
        if let MadridRecvMessage {
            command: 130,
            sender: Some(sender),
            target: Some(target),
            token: Some(sender_token),
            ..
        } = &payload {
            let mut cache_lock = self.identity.cache.lock().await;
            cache_lock.invalidate(&target, &sender);
            return Ok(if sender == target {
                self.identity.ensure_private_self(&mut cache_lock, &target, true).await?;
                let private_self = &cache_lock.cache.get(target).unwrap().private_data;

                let Some(new_device_token) = private_self.iter().find(|dev| &dev.token == sender_token) else {
                    error!("New device not found!");
                    return Ok(None)
                };

                if new_device_token.identites.len() != self.identity.get_handles().await.len() {
                    info!("New handles; reregistering!");
                    self.identity.refresh().await?;
                }

                payload.to_message(None, Message::PeerCacheInvalidate).ok()
            } else {
                None
            })
        }

        if let MadridRecvMessage {
            command: 145,
            no_reply: None | Some(false),
            sender: Some(sender),
            ..
        } = &payload {
            let _ = self.send(&mut MessageInst::new(ConversationData {
                participants: vec![sender.clone()],
                cv_name: None,
                sender_guid: Some(Uuid::new_v4().to_string()),
                after_guid: None,
            }, &sender, Message::MessageReadOnDevice)).await;
        }

        if payload.message_unenc.is_none() {
            let MadridRecvMessage {
                sender: Some(sender),
                target: Some(target),
                message: Some(message),
                token: Some(token),
                verification_failed,
                .. 
            } = &mut payload else { return Ok(None) };
            let ident = match self.identity.get_key_for_sender(&target, &sender, &token).await {
                Ok(ident) => Some(ident.client_data.public_message_identity_key),
                Err(err) => {
                    error!("No identity for payload! {}", err);
                    *verification_failed = true;
                    None
                }
            };

            let decrypted = self.identity.decrypt_payload(ident.as_ref(), &message)?;
            let ungzipped = ungzip(&decrypted).unwrap_or_else(|_| decrypted);

            let parsed: Value = plist::from_bytes(&ungzipped)?;
            payload.message_unenc = Some(parsed);
        }

        match MessageInst::from_raw(payload.message_unenc.take().unwrap(), &payload, &self.conn).await {
            Err(PushError::BadMsg) => Ok(None),
            Err(err) => Err(err),
            Ok(msg) => Ok(Some(msg))
        }
    }

    pub async fn send(&self, message: &mut MessageInst) -> Result<SendJob, PushError> {
        let handles = self.identity.get_handles().await;

        let targets = message.prepare_send(&handles);
        self.identity.cache_keys(
            &targets,
            message.sender.as_ref().unwrap(),
            false,
            &QueryOptions { required_for_message: true, result_expected: true }
        ).await?;

        let handle = message.sender.as_ref().unwrap().to_string();
        let ident_cache = self.identity.cache.lock().await;
        let mut message_targets = if let Some(message_targets) = &message.target {
            ident_cache.get_targets(&handle, &targets, message_targets)?
        } else {
            ident_cache.get_participants_targets(&handle, &targets)
        };

        // do not send to self
        let my_token = self.conn.get_token().await;
        message_targets.retain(|target| &target.delivery_data.push_token != &my_token);

        if message_targets.is_empty() {
            return Ok(SendJob {
                process: tokio::sync::broadcast::channel(1).1,
                handle: None,
            })
        }
        
        let (sender, receiver) = 
            tokio::sync::broadcast::channel(message_targets.len());
        
        let mut progress = receiver.resubscribe();

        let job = InnerSendJob {
            conn: self.conn.clone(),
            identity: self.identity.clone(),
            user_agent: self.os_config.get_version_ua(),
            message: message.clone(),
            status: sender,
        };

        let mut job_spawned = tokio::spawn(job.send_targets(message_targets, 0));

        let mut received = false;
        let mut checked = false;
        loop {
            select! {
                finished = &mut job_spawned => {
                    finished.unwrap()?;
                    checked = true;
                    received = true; // for no confirm items
                    break; // Done
                },
                _prog = progress.recv() => {
                    received = true;
                },
                _time = tokio::time::sleep(Duration::from_millis(if received { 500 } else { 15000 })) => {
                    break;
                }
            }
        }

        if !received {
            debug!("Not received");
            job_spawned.abort();
            return Err(PushError::SendTimedOut)
        }
        
        Ok(SendJob {
            process: receiver,
            handle: if checked { None } else { Some(job_spawned) },
        })
    }
}

#[derive(Clone, Copy)]
pub enum SendResult {
    Sent,
    APSError(i64),
    TimedOut,
}

pub struct SendJob {
    pub process: tokio::sync::broadcast::Receiver<(DeliveryHandle, SendResult)>,
    pub handle: Option<JoinHandle<Result<(), PushError>>>,
}

struct InnerSendJob {
    pub conn: APSConnection,
    pub identity: IdentityManager,
    pub user_agent: String,
    pub message: MessageInst,
    pub status: tokio::sync::broadcast::Sender<(DeliveryHandle, SendResult)>,
}

impl InnerSendJob {
    #[async_recursion]
    async fn send_targets(self, targets: Vec<DeliveryHandle>, retry_count: u8) -> Result<(), PushError> {
        info!("Sending retry {}", retry_count);
        let message = &self.message;
        let handle = message.sender.as_ref().unwrap().to_string();
        let my_handles = self.identity.get_handles().await;
        let raw = if message.has_payload() { Some(message.to_raw(&my_handles, &self.conn).await?) } else { None };

        let mut groups = vec![];
        let mut group = vec![];
        let mut group_size = 0;
        const GROUP_MAX_SIZE: usize = 10000;

        for target in &targets {
            let encrypted = if let Some(msg) = &raw {
                Some(self.identity.encrypt_payload(&target.delivery_data.client_data.public_message_identity_key, &msg)?)
            } else { None };
            let send_delivered = if message.send_delivered { &target.participant != message.sender.as_ref().unwrap() } else { false };
            group_size += encrypted.as_ref().map(|i| i.len()).unwrap_or(0);
            group.push(target.build_bundle(send_delivered, encrypted));
            
            if group_size > GROUP_MAX_SIZE {
                groups.push(std::mem::take(&mut group));
                group_size = 0;
            }
        }
        if group.len() > 0 {
            groups.push(group);
        }

        let mut messages = self.conn.subscribe().await;

        let msg_id = rand::thread_rng().next_u32();
        let uuid = Uuid::from_str(&message.id).unwrap().as_bytes().to_vec();
        debug!("send_uuid {}", encode_hex(&uuid));
        for (batch, group) in groups.into_iter().enumerate() {
            let complete = SendMessage {
                batch: batch as u8 + 1,
                command: message.message.get_c(),
                encryption: if message.has_payload() { Some("pair".to_string()) } else { None },
                user_agent: self.user_agent.clone(),
                v: 8,
                message_id: msg_id,
                uuid: uuid.clone().into(),
                payloads: group,
                sender: message.sender.clone().unwrap(),
                ex: message.get_ex(),
                no_response: message.message.get_nr(),
                retry_count: if retry_count != 0 { Some(retry_count) } else { None },
                original_epoch_nanos: if retry_count != 0 { Some(message.sent_timestamp * 1000000) } else { None },
            };
    
            let binary = plist_to_bin(&complete)?;
            self.conn.send_message(if message.message.is_sms() { "com.apple.private.alloy.sms" } else { "com.apple.madrid" }, binary, Some(msg_id)).await?
        }

        if message.message.get_nr() != Some(true) {
            let mut remain_targets = targets;
            let mut refresh_targets: Vec<DeliveryHandle> = vec![];
            let payloads_cnt = remain_targets.len();
            info!("payload {payloads_cnt}");

            while !remain_targets.is_empty() {
                let filter = get_message(|load| {
                    debug!("got {:?}", load);
                    let result: MadridRecvMessage = plist::from_value(&load).ok()?;
                    if result.command != 255 {
                        return None
                    }
                    // make sure it's my message
                    if result.uuid.as_ref() == Some(&uuid) { Some(result) } else { None }
                }, &["com.apple.madrid", "com.apple.private.alloy.sms"]);

                let Ok(msg) = tokio::time::timeout(std::time::Duration::from_secs(60 * ((retry_count as u64) + 1)), 
                    self.conn.wait_for(&mut messages, filter)).await else {
                    break;
                };
                let load = msg?;

                let Some(target_idx) = remain_targets.iter().position(|target| Some(&target.delivery_data.push_token) == load.token.as_ref()) else { continue };
                match load.status.unwrap() {
                    5032 => {
                        info!("got 5032, refreshing keys!");
                        refresh_targets.push(remain_targets.remove(target_idx));
                    },
                    0 | 5008 => {
                        let _ = self.status.send((remain_targets.remove(target_idx), SendResult::Sent)); // succeeded
                    },
                    _status => {
                        if remain_targets[target_idx].participant == handle {
                            warn!("Failed to deliver to self device; ignoring!");
                            continue // ignore errors sending to self devices
                        }
                        let _ = self.status.send((remain_targets.remove(target_idx), SendResult::APSError(_status)));
                    }
                }
            }
            
            if !remain_targets.is_empty() || !refresh_targets.is_empty() {
                // will bail early if refresh_targets is empty
                let new_targets = self.identity.refresh_handles(&handle, &refresh_targets).await?;
                remain_targets.extend(new_targets);

                if retry_count == 5 {
                    for target in remain_targets {
                        let _ = self.status.send((target, SendResult::TimedOut));
                    }
                    info!("Retry failed");
                    return Ok(())
                }

                self.send_targets(remain_targets, retry_count + 1).await?;
            }
        }
        info!("Sending done!");
        Ok(())
    }
}


