// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use std::{collections::HashMap, fmt::Debug, hash::Hash, mem, sync::Arc, time::Duration};

pub use bluer::DeviceProperty;
use bluer::{
    agent::{Agent, AgentHandle},
    Adapter, Address, Session, Uuid,
};

use cosmic::{
    iced::{
        self,
        futures::{SinkExt, StreamExt},
        Subscription,
    },
    iced_futures::stream,
};

use futures::executor::block_on;
use rand::Rng;
use tokio::{
    spawn,
    sync::{
        mpsc::{channel, Receiver, Sender},
        Mutex,
    },
    task::JoinHandle,
    time::timeout,
};

// Copied from https://github.com/bluez/bluez/blob/39467578207889fd015775cbe81a3db9dd26abea/src/dbus-common.c#L53
fn device_type_to_icon(device_type: &str) -> &'static str {
    match device_type {
        "computer" => "laptop-symbolic",
        "phone" => "smartphone-symbolic",
        "network-wireless" => "network-wireless-symbolic",
        "audio-headset" => "audio-headset-symbolic",
        "audio-headphones" => "audio-headphones-symbolic",
        "camera-video" => "camera-video-symbolic",
        "audio-card" => "audio-card-symbolic",
        "input-gaming" => "input-gaming-symbolic",
        "input-keyboard" => "input-keyboard-symbolic",
        "input-tablet" => "input-tablet-symbolic",
        "input-mouse" => "input-mouse-symbolic",
        "printer" => "printer-network-symbolic",
        "camera-photo" => "camera-photo-symbolic",
        _ => DEFAULT_DEVICE_ICON,
    }
}

pub fn bluetooth_subscription<I: 'static + Hash + Copy + Send + Sync + Debug>(
    id: I,
) -> iced::Subscription<BluerEvent> {
    Subscription::run_with_id(
        id,
        stream::channel(50, move |mut output| async move {
            let mut state = State::Ready(0);

            loop {
                state = start_listening(state, &mut output).await;
            }
        }),
    )
}

pub enum State {
    Ready(u32),
    Waiting { session_state: BluerSessionState },
    Finished,
}

async fn start_listening(
    state: State,
    output: &mut futures::channel::mpsc::Sender<BluerEvent>,
) -> State {
    match state {
        State::Ready(retry_count) => {
            let session = match Session::new().await {
                Ok(s) => s,
                Err(_) => {
                    _ = tokio::time::sleep(Duration::from_millis(
                        2_u64.saturating_pow(retry_count),
                    ))
                    .await;

                    return State::Ready(retry_count.saturating_add(1));
                }
            };

            let session_state = match BluerSessionState::new(session).await {
                Ok(s) => s,
                Err(_) => {
                    _ = tokio::time::sleep(Duration::from_millis(
                        2_u64.saturating_pow(retry_count),
                    ))
                    .await;
                    return State::Ready(retry_count.saturating_add(1));
                }
            };

            let state = session_state.bluer_state().await;
            // reconnect to paired and trusted devices
            if state.bluetooth_enabled {
                for d in &state.devices {
                    if d.paired_and_trusted() {
                        _ = session_state
                            .req_tx
                            .send(BluerRequest::ConnectDevice(d.address))
                            .await;
                    }
                }
            }
            _ = output
                .send(BluerEvent::Init {
                    sender: session_state.req_tx.clone(),
                    state: state.clone(),
                })
                .await;
            State::Waiting { session_state }
        }
        State::Waiting { mut session_state } => {
            let mut session_rx = match session_state.rx.take() {
                Some(rx) => rx,
                None => {
                    _ = output.send(BluerEvent::Finished).await;
                    return State::Finished;
                }
            };

            if let Some(event) = session_rx.recv().await {
                match event {
                    BluerSessionEvent::ChangesProcessed(state) => {
                        _ = output.send(BluerEvent::DevicesChanged { state }).await;
                    }
                    BluerSessionEvent::RequestResponse {
                        req,
                        state,
                        err_msg,
                    } => {
                        _ = output
                            .send(BluerEvent::RequestResponse {
                                req,
                                state,
                                err_msg,
                            })
                            .await;
                    }
                    BluerSessionEvent::AgentEvent(e) => {
                        _ = output.send(BluerEvent::AgentEvent(e)).await;
                    }
                    _ => {}
                }
            } else {
                _ = output.send(BluerEvent::Finished).await;
                return State::Finished;
            };
            session_state.rx = Some(session_rx);
            State::Waiting { session_state }
        }
        State::Finished => iced::futures::future::pending().await,
    }
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub enum BluerRequest {
    SetBluetoothEnabled(bool),
    PairDevice(Address),
    ConnectDevice(Address),
    DisconnectDevice(Address),
    CancelConnect(Address),
    StateUpdate,
}

#[derive(Debug, Clone)]
pub enum BluerEvent {
    RequestResponse {
        req: BluerRequest,
        state: BluerState,
        err_msg: Option<String>,
    },
    Init {
        sender: Sender<BluerRequest>,
        state: BluerState,
    },
    DevicesChanged {
        state: BluerState,
    },
    AgentEvent(BluerAgentEvent),
    Finished,
}

#[derive(Debug, Clone, Default)]
pub struct BluerState {
    pub devices: Vec<BluerDevice>,
    pub bluetooth_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum BluerDeviceStatus {
    Connected,
    Connecting,
    Paired,
    /// Pairing is in progress, maybe with a passkey or pincode
    /// passkey or pincode will be 000000 - 999999
    Pairing,
    Disconnected,
    Disconnecting,
}

#[derive(Debug, Clone)]
pub struct BluerDevice {
    pub name: String,
    pub address: Address,
    pub status: BluerDeviceStatus,
    pub properties: Vec<DeviceProperty>,
    pub icon: String,
}

impl Eq for BluerDevice {}

impl Ord for BluerDevice {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match self.status.cmp(&other.status) {
            std::cmp::Ordering::Equal => self.name.to_lowercase().cmp(&other.name.to_lowercase()),
            o => o,
        }
    }
}

impl PartialOrd for BluerDevice {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match self.status.cmp(&other.status) {
            std::cmp::Ordering::Equal => {
                Some(self.name.to_lowercase().cmp(&other.name.to_lowercase()))
            }
            o => Some(o),
        }
    }
}

impl PartialEq for BluerDevice {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.address == other.address
    }
}

const DEFAULT_DEVICE_ICON: &str = "bluetooth-symbolic";

impl BluerDevice {
    pub async fn from_device(device: &bluer::Device) -> Self {
        let mut name = device
            .name()
            .await
            .unwrap_or_default()
            .unwrap_or_else(|| device.address().to_string());
        if name.is_empty() {
            name = device.address().to_string();
        };
        let is_paired = device.is_paired().await.unwrap_or_default();
        let is_connected = device.is_connected().await.unwrap_or_default();
        let properties = device.all_properties().await.unwrap_or_default();
        let status = if is_connected {
            BluerDeviceStatus::Connected
        } else if is_paired {
            BluerDeviceStatus::Paired
        } else {
            BluerDeviceStatus::Disconnected
        };
        let icon = properties
            .iter()
            .find_map(|p| {
                if let DeviceProperty::Icon(icon) = p {
                    Some(device_type_to_icon(icon.clone().as_str()).to_string())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| device_type_to_icon(DEFAULT_DEVICE_ICON).to_string());

        Self {
            name,
            address: device.address(),
            status,
            properties,
            icon,
        }
    }

    fn paired_and_trusted(&self) -> bool {
        self.properties
            .iter()
            .filter(|p| {
                matches!(
                    p,
                    DeviceProperty::Trusted(true) | DeviceProperty::Paired(true)
                )
            })
            .count()
            == 2
    }

    #[must_use]
    pub fn is_known_device_type(&self) -> bool {
        self.icon != DEFAULT_DEVICE_ICON
    }

    #[must_use]
    pub fn has_name(&self) -> bool {
        self.name != self.address.to_string()
    }
}

#[derive(Debug, Clone)]
pub enum BluerSessionEvent {
    RequestResponse {
        req: BluerRequest,
        state: BluerState,
        err_msg: Option<String>,
    },
    ChangesProcessed(BluerState),
    ChangeStreamEnded, // TODO can we just restart the stream in a new task?
    AgentEvent(BluerAgentEvent),
}

#[derive(Debug, Clone)]
pub enum BluerAgentEvent {
    DisplayPinCode(BluerDevice, String),
    DisplayPasskey(BluerDevice, String),
    RequestPinCode(BluerDevice),
    RequestPasskey(BluerDevice),
    RequestConfirmation(BluerDevice, String, Sender<bool>), // Note mpsc channel is used bc the sender must be cloned in the iced Message machinery
    RequestDeviceAuthorization(BluerDevice, Sender<bool>),
    RequestServiceAuthorization(BluerDevice, Uuid, Sender<bool>),
}

pub struct BluerSessionState {
    _session: Session,
    _agent_handle: AgentHandle,
    pub adapter: Adapter,
    pub rx: Option<Receiver<BluerSessionEvent>>,
    pub req_tx: Sender<BluerRequest>,
    wake_up_discover_tx: Sender<()>,
    wake_up_discover_rx: Option<Receiver<()>>,
    tx: Sender<BluerSessionEvent>,
    active_requests: Arc<Mutex<HashMap<BluerRequest, JoinHandle<anyhow::Result<()>>>>>,
}

impl BluerSessionState {
    pub(crate) async fn new(session: Session) -> anyhow::Result<Self> {
        let adapter = session.default_adapter().await?;
        let devices = build_device_list(&adapter).await;
        let (tx, rx) = tokio::sync::mpsc::channel(100);
        let (req_tx, req_rx) = channel(100);
        let tx_clone_1 = tx.clone();
        let tx_clone_2 = tx.clone();
        let tx_clone_3 = tx.clone();
        let tx_clone_4 = tx.clone();
        let tx_clone_5 = tx.clone();
        let tx_clone_6 = tx.clone();
        let tx_clone_7 = tx.clone();
        let adapter_clone_1 = adapter.clone();
        let adapter_clone_2 = adapter.clone();
        let adapter_clone_3 = adapter.clone();
        let adapter_clone_4 = adapter.clone();
        let adapter_clone_5 = adapter.clone();
        let adapter_clone_6 = adapter.clone();
        let adapter_clone_7 = adapter.clone();

        let _agent = Agent {
            request_default: false, // TODO which agent should eventually become the default? Maybe the one in the settings app?
            request_pin_code: Some(Box::new(move |req| {
                let agent_clone = adapter_clone_1.clone();
                let tx_clone = tx_clone_1.clone();
                Box::pin(async move {
                    let device = match agent_clone.device(req.device) {
                        Ok(d) => d,
                        Err(_) => return Err(bluer::agent::ReqError::Rejected),
                    };
                    let _ = tx_clone
                        .send(BluerSessionEvent::AgentEvent(
                            BluerAgentEvent::RequestPinCode(
                                BluerDevice::from_device(&device).await,
                            ),
                        ))
                        .await;
                    let mut rng = rand::rng();
                    let pin_code = rng.random_range(0..999999);
                    Ok(format!("{:06}", pin_code))
                })
            })),
            display_pin_code: Some(Box::new(move |req| {
                let agent_clone = adapter_clone_2.clone();
                let tx_clone = tx_clone_2.clone();
                Box::pin(async move {
                    let device = match agent_clone.device(req.device) {
                        Ok(d) => d,
                        Err(_) => return Err(bluer::agent::ReqError::Rejected),
                    };
                    let _ = tx_clone
                        .send(BluerSessionEvent::AgentEvent(
                            BluerAgentEvent::DisplayPinCode(
                                BluerDevice::from_device(&device).await,
                                req.pincode,
                            ),
                        ))
                        .await;

                    Ok(())
                })
            })),
            request_passkey: Some(Box::new(move |req| {
                let agent_clone = adapter_clone_3.clone();
                let tx_clone = tx_clone_3.clone();
                Box::pin(async move {
                    let device = match agent_clone.device(req.device) {
                        Ok(d) => d,
                        Err(_) => return Err(bluer::agent::ReqError::Rejected),
                    };
                    let _ = tx_clone
                        .send(BluerSessionEvent::AgentEvent(
                            BluerAgentEvent::RequestPasskey(
                                BluerDevice::from_device(&device).await,
                            ),
                        ))
                        .await;
                    let mut rng = rand::rng();
                    let pin_code = rng.random_range(0..999999);
                    Ok(pin_code)
                })
            })),
            display_passkey: Some(Box::new(move |req| {
                let agent_clone = adapter_clone_4.clone();
                let tx_clone = tx_clone_4.clone();
                Box::pin(async move {
                    let device = match agent_clone.device(req.device) {
                        Ok(d) => d,
                        Err(_) => return Err(bluer::agent::ReqError::Rejected),
                    };
                    let _ = tx_clone
                        .send(BluerSessionEvent::AgentEvent(
                            BluerAgentEvent::DisplayPasskey(
                                BluerDevice::from_device(&device).await,
                                format!("{:06}", req.passkey),
                            ),
                        ))
                        .await;
                    Ok(())
                })
            })),
            request_confirmation: Some(Box::new(move |req| {
                let agent_clone = adapter_clone_5.clone();
                let tx_clone = tx_clone_5.clone();
                Box::pin(async move {
                    let device = match agent_clone.device(req.device) {
                        Ok(d) => d,
                        Err(_) => return Err(bluer::agent::ReqError::Rejected),
                    };
                    let (tx, mut rx) = channel(1);
                    let _ = tx_clone
                        .send(BluerSessionEvent::AgentEvent(
                            BluerAgentEvent::RequestConfirmation(
                                BluerDevice::from_device(&device).await,
                                format!("{:06}", req.passkey),
                                tx,
                            ),
                        ))
                        .await;
                    let res = rx.recv().await;
                    match res {
                        Some(res) if res => Ok(()),
                        _ => Err(bluer::agent::ReqError::Rejected),
                    }
                })
            })),
            request_authorization: Some(Box::new(move |req| {
                let agent_clone = adapter_clone_6.clone();
                let tx_clone = tx_clone_6.clone();
                Box::pin(async move {
                    let device = match agent_clone.device(req.device) {
                        Ok(d) => d,
                        Err(_) => return Err(bluer::agent::ReqError::Rejected),
                    };
                    let (tx, mut rx) = channel(1);
                    let _ = tx_clone
                        .send(BluerSessionEvent::AgentEvent(
                            BluerAgentEvent::RequestDeviceAuthorization(
                                BluerDevice::from_device(&device).await,
                                tx,
                            ),
                        ))
                        .await;
                    let res = rx.recv().await;
                    match res {
                        Some(res) if res => Ok(()),
                        _ => Err(bluer::agent::ReqError::Rejected),
                    }
                })
            })),
            authorize_service: Some(Box::new(move |req| {
                let agent_clone = adapter_clone_7.clone();
                let tx_clone = tx_clone_7.clone();
                Box::pin(async move {
                    let device = match agent_clone.device(req.device) {
                        Ok(d) => d,
                        Err(_) => return Err(bluer::agent::ReqError::Rejected),
                    };
                    let (tx, mut rx) = channel(1);
                    // TODO better describe the service to the user
                    let _ = tx_clone
                        .send(BluerSessionEvent::AgentEvent(
                            BluerAgentEvent::RequestServiceAuthorization(
                                BluerDevice::from_device(&device).await,
                                req.service,
                                tx,
                            ),
                        ))
                        .await;
                    let res = rx.recv().await;
                    match res {
                        Some(res) if res => Ok(()),
                        _ => Err(bluer::agent::ReqError::Rejected),
                    }
                })
            })),
            _non_exhaustive: (),
        };
        let _agent_handle = session.register_agent(_agent).await?;
        let (wake_up_discover_tx, wake_up_discover_rx) = channel(10);
        let mut self_ = Self {
            _agent_handle,
            _session: session,
            adapter,
            rx: Some(rx),
            req_tx,
            wake_up_discover_rx: Some(wake_up_discover_rx),
            wake_up_discover_tx,
            tx,
            active_requests: Arc::new(Mutex::new(HashMap::new())),
        };
        self_.process_requests(req_rx);
        self_.process_changes();
        self_.listen_bluetooth_power_changes();

        Ok(self_)
    }

    fn listen_bluetooth_power_changes(&self) {
        let tx = self.tx.clone();
        let req_tx = self.req_tx.clone();
        let adapter_clone = self.adapter.clone();
        let wake_up_discover_tx = self.wake_up_discover_tx.clone();
        let _handle: JoinHandle<anyhow::Result<()>> = spawn(async move {
            let mut status = adapter_clone.is_powered().await.unwrap_or_default();
            loop {
                _ = tokio::time::sleep(Duration::from_secs(5)).await;
                let new_status = adapter_clone.is_powered().await.unwrap_or_default();
                if new_status != status {
                    status = new_status;
                    let state = BluerState {
                        devices: build_device_list(&adapter_clone).await,
                        bluetooth_enabled: status,
                    };
                    if state.bluetooth_enabled {
                        for d in &state.devices {
                            if d.paired_and_trusted() {
                                _ = req_tx.send(BluerRequest::ConnectDevice(d.address)).await;
                            }
                        }
                    }
                    _ = wake_up_discover_tx.send(()).await;
                    let _ = tx.send(BluerSessionEvent::ChangesProcessed(state)).await;
                }
            }
        });
    }

    pub(crate) fn process_changes(&mut self) {
        let tx = self.tx.clone();
        let req_tx = self.req_tx.clone();
        let Some(mut wake_up) = self.wake_up_discover_rx.take() else {
            tracing::error!("Failed to take wake up channel");
            return;
        };
        let adapter_clone = self.adapter.clone();
        let _monitor_devices: tokio::task::JoinHandle<Result<(), anyhow::Error>> = spawn(
            async move {
                loop {
                    let mut milli_timeout = 10;

                    let mut change_stream = {
                        let mut res = adapter_clone.discover_devices_with_changes().await;
                        while res.is_err() {
                            _ = tokio::time::timeout(
                                Duration::from_millis(milli_timeout),
                                wake_up.recv(),
                            )
                            .await;
                            res = adapter_clone.discover_devices_with_changes().await;
                            milli_timeout = milli_timeout.saturating_mul(2);
                        }
                        milli_timeout = 10;
                        res.unwrap()
                    };
                    let mut devices: Vec<BluerDevice> = Vec::new();
                    'outer: loop {
                        tokio::select! {
                            change = timeout(Duration::from_millis(milli_timeout), change_stream.next()) => {
                                if let Ok(e) = change {
                                    if e.is_none() {
                                        break 'outer;
                                    }
                                } else {
                                    milli_timeout = milli_timeout.saturating_mul(2);
                                    continue;
                                }
                            }
                            _wake = wake_up.recv() => {
                            }
                        };

                        let mut new_devices = build_device_list(&adapter_clone).await;
                        for d in new_devices
                            .iter()
                            .filter(|d| !devices.contains(d) && d.paired_and_trusted())
                        {
                            _ = req_tx.send(BluerRequest::ConnectDevice(d.address)).await;
                        }
                        devices = mem::take(&mut new_devices);

                        let _ = tx
                            .send(BluerSessionEvent::ChangesProcessed(BluerState {
                                devices: build_device_list(&adapter_clone).await,
                                bluetooth_enabled: adapter_clone
                                    .is_powered()
                                    .await
                                    .unwrap_or_default(),
                            }))
                            .await;
                        // reset timeout
                        milli_timeout = 10;
                    }
                    let _ = tx.send(BluerSessionEvent::ChangeStreamEnded).await;
                }
            },
        );
    }

    pub(crate) fn process_requests(&self, request_rx: Receiver<BluerRequest>) {
        let active_requests = self.active_requests.clone();
        let adapter = self.adapter.clone();
        let tx = self.tx.clone();
        let wake_up_tx = self.wake_up_discover_tx.clone();

        let _handle: JoinHandle<anyhow::Result<()>> = spawn(async move {
            let mut request_rx = request_rx;

            while let Some(req) = request_rx.recv().await {
                let req_clone = req.clone();
                let req_clone_2 = req.clone();
                let active_requests_clone = active_requests.clone();
                let tx_clone = tx.clone();
                let adapter_clone = adapter.clone();
                let wake_up_tx = wake_up_tx.clone();

                let handle = spawn(async move {
                    let mut err_msg = None;
                    match &req_clone {
                        BluerRequest::SetBluetoothEnabled(enabled) => {
                            if let Err(e) = adapter_clone.set_powered(*enabled).await {
                                tracing::error!("Failed to power off bluetooth adapter. {e:?}")
                            }

                            // rfkill will be persisted after reboot
                            let name = adapter_clone.name();
                            if let Some(id) = tokio::process::Command::new("rfkill")
                                .arg("list")
                                .arg("-n")
                                .arg("--output")
                                .arg("ID,DEVICE")
                                .output()
                                .await
                                .ok()
                                .and_then(|o| {
                                    let lines = String::from_utf8(o.stdout).ok()?;
                                    lines.split("\n").into_iter().find_map(|row| {
                                        let (id, cname) = row.trim().split_once(" ")?;
                                        (name == cname).then_some(id.to_string())
                                    })
                                })
                            {
                                if let Err(err) = tokio::process::Command::new("rfkill")
                                    .arg(if *enabled { "unblock" } else { "block" })
                                    .arg(id)
                                    .output()
                                    .await
                                {
                                    tracing::error!(
                                        "Failed to set bluetooth state using rfkill. {err:?}"
                                    );
                                }
                            }

                            if *enabled {
                                _ = wake_up_tx.send(()).await;
                            }
                        }
                        BluerRequest::PairDevice(address) => {
                            let res = adapter_clone.device(*address);
                            if let Err(err) = res {
                                err_msg = Some(err.to_string());
                            } else if let Ok(device) = res {
                                let res = device.pair().await;
                                if let Err(err) = res {
                                    err_msg = Some(err.to_string());
                                } else {
                                    if let Err(err) = device.set_trusted(true).await {
                                        tracing::error!(?err, "Failed to trust device.");
                                    }
                                }
                            }
                        }
                        BluerRequest::ConnectDevice(address) => {
                            let res = adapter_clone.device(*address);
                            if let Err(err) = res {
                                err_msg = Some(err.to_string());
                            } else if let Ok(device) = res {
                                let res = device.connect().await;
                                if let Err(err) = res {
                                    err_msg = Some(err.to_string());
                                } else {
                                    if let Err(err) = device.set_trusted(true).await {
                                        tracing::error!(?err, "Failed to trust device.");
                                    }
                                }
                            }
                        }
                        BluerRequest::DisconnectDevice(address) => {
                            let res = adapter_clone.device(*address);
                            if let Err(err) = res {
                                err_msg = Some(err.to_string());
                            } else if let Ok(device) = res {
                                let res = device.disconnect().await;
                                if let Err(err) = res {
                                    err_msg = Some(err.to_string());
                                }
                            }
                        }
                        BluerRequest::CancelConnect(_) => {
                            if let Some(handle) = active_requests_clone.lock().await.get(&req_clone)
                            {
                                handle.abort();
                            } else {
                                err_msg = Some("No active connection request found".to_string());
                            }
                        }
                        BluerRequest::StateUpdate => {}
                    };

                    let state = BluerState {
                        devices: build_device_list(&adapter_clone).await,
                        bluetooth_enabled: adapter_clone.is_powered().await.unwrap_or_default(),
                    };

                    let _ = tx_clone
                        .send(BluerSessionEvent::RequestResponse {
                            req: req_clone,
                            state,
                            err_msg,
                        })
                        .await;

                    active_requests_clone.lock().await.remove(&req_clone_2);

                    Ok(())
                });

                active_requests.lock().await.insert(req, handle);
            }
            Ok(())
        });
    }

    pub(crate) async fn bluer_state(&self) -> BluerState {
        BluerState {
            devices: build_device_list(&self.adapter).await,
            // TODO is this a proper way of checking if bluetooth is enabled?
            bluetooth_enabled: self.adapter.is_powered().await.unwrap_or_default(),
        }
    }
}

async fn build_device_list(adapter: &Adapter) -> Vec<BluerDevice> {
    let addrs = adapter.device_addresses().await.unwrap_or_default();
    let mut devices = Vec::with_capacity(addrs.len());

    for address in addrs {
        let device = match adapter.device(address) {
            Ok(device) => device,
            Err(_) => continue,
        };

        devices.push(BluerDevice::from_device(&device).await);
    }
    devices.sort();
    devices
}
