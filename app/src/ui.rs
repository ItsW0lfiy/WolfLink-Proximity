use super::*;
use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::c_void;
use std::iter;
use std::ptr::null;
use windows_sys::Win32::Foundation::{GlobalFree, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{COLOR_WINDOW, DEFAULT_GUI_FONT, GetStockObject};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const ID_USER: u16 = 101;
const ID_EXPERIMENTAL: u16 = 102;
const ID_ACK: u16 = 103;
const ID_CREATE: u16 = 104;
const ID_JOIN: u16 = 105;
const ID_LEAVE: u16 = 106;
const ID_INVITE: u16 = 107;
const ID_COPY: u16 = 108;
const ID_MUTE: u16 = 109;
const ID_LOUDNESS: u16 = 110;
const ID_VOLUME_SET: u16 = 111;
const ID_X: u16 = 112;
const ID_Y: u16 = 113;
const ID_Z: u16 = 114;
const ID_POSITION: u16 = 115;
const ID_UNAVAILABLE: u16 = 116;
const ID_UPDATE_NOTES: u16 = 117;
const ID_UPDATE_NOW: u16 = 118;

thread_local! { static STATE: RefCell<Option<Ui>> = const { RefCell::new(None) }; }

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(iter::once(0)).collect()
}

unsafe fn label(hwnd: HWND, text: &str, x: i32, y: i32, width: i32) -> HWND {
    unsafe {
        control(
            hwnd,
            "STATIC",
            text,
            WS_CHILD | WS_VISIBLE,
            x,
            y,
            width,
            22,
            0,
        )
    }
}

unsafe fn control(
    parent: HWND,
    class: &str,
    text: &str,
    style: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    id: u16,
) -> HWND {
    let class = wide(class);
    let title = wide(text);
    let style = if id == 0 { style } else { style | WS_TABSTOP };
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            style,
            x,
            y,
            width,
            height,
            parent,
            id as usize as *mut c_void,
            null_mut(),
            null(),
        )
    };
    if !hwnd.is_null() {
        unsafe {
            SendMessageW(
                hwnd,
                WM_SETFONT,
                GetStockObject(DEFAULT_GUI_FONT) as usize,
                1,
            );
        }
    }
    hwnd
}

use std::ptr::null_mut;

fn text(hwnd: HWND) -> String {
    let mut buffer = vec![0u16; 4097];
    let count = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    String::from_utf16_lossy(&buffer[..count.max(0) as usize])
}

fn set_text(hwnd: HWND, value: &str) {
    let value = wide(value);
    unsafe {
        SetWindowTextW(hwnd, value.as_ptr());
    }
}

fn checked(hwnd: HWND) -> bool {
    unsafe { SendMessageW(hwnd, BM_GETCHECK, 0, 0) == 1 }
}

fn clipboard(hwnd: HWND, value: &str) -> Result<(), &'static str> {
    let data = wide(value);
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return Err("Cannot open clipboard");
        }
        let _ = EmptyClipboard();
        let handle = GlobalAlloc(GMEM_MOVEABLE, data.len() * 2);
        if handle.is_null() {
            let _ = CloseClipboard();
            return Err("Clipboard allocation failed");
        }
        let ptr = GlobalLock(handle) as *mut u16;
        if ptr.is_null() {
            let _ = GlobalFree(handle);
            let _ = CloseClipboard();
            return Err("Clipboard lock failed");
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        let _ = GlobalUnlock(handle);
        if SetClipboardData(13, handle).is_null() {
            let _ = GlobalFree(handle);
            let _ = CloseClipboard();
            return Err("Clipboard copy failed");
        }
        let _ = CloseClipboard();
    }
    Ok(())
}

struct Ui {
    hwnd: HWND,
    actions: Sender<Action>,
    events: Receiver<Event>,
    preferences: Preferences,
    username: HWND,
    experimental: HWND,
    acknowledged: HWND,
    invitation_input: HWND,
    invitation: String,
    mute: HWND,
    loudness: HWND,
    positions: [HWND; 3],
    tailnet: HWND,
    status: HWND,
    roblox_state: HWND,
    participants_view: HWND,
    log_view: HWND,
    logs: Vec<String>,
    participants: HashSet<String>,
    connected: bool,
    hosting: bool,
    room_mode: RoomMode,
    telemetry_valid: bool,
    update_status: HWND,
    update_offer: Option<CheckResult>,
    update_due: Option<Instant>,
    update_notes: String,
    mandatory_update: bool,
    updating: bool,
}

impl Ui {
    fn new(hwnd: HWND) -> Self {
        let (actions, receiver) = mpsc::channel();
        let (sender, events) = mpsc::channel();
        thread::spawn(move || worker(receiver, sender));
        let preferences = load_preferences();
        unsafe {
            label(
                hwnd,
                concat!("WolfLink Proximity ", env!("CARGO_PKG_VERSION"), " — alpha"),
                20,
                15,
                700,
            );
            let tailnet = label(hwnd, "Tailscale: checking…", 20, 43, 730);
            let status = label(hwnd, "Discord/relay: not connected", 20, 67, 730);
            label(
                hwnd,
                "Connection: Tailscale relay · Direct P2P: not available",
                20,
                91,
                730,
            );
            label(hwnd, "Your Roblox username", 20, 125, 210);
            let username = control(
                hwnd,
                "EDIT",
                &preferences.roblox_username,
                WS_CHILD | WS_VISIBLE | WS_BORDER | ES_AUTOHSCROLL as u32,
                230,
                121,
                250,
                26,
                ID_USER,
            );
            let experimental = control(
                hwnd,
                "BUTTON",
                "Experimental private room — Unverified Roblox Server",
                WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32,
                20,
                158,
                690,
                25,
                ID_EXPERIMENTAL,
            );
            let acknowledged = control(
                hwnd,
                "BUTTON",
                "We are responsible for joining the same Roblox server",
                WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32,
                20,
                185,
                690,
                25,
                ID_ACK,
            );
            label(
                hwnd,
                "Different Roblox servers can give misleading distances. Server identity is not verified.",
                20,
                211,
                740,
            );
            control(
                hwnd,
                "BUTTON",
                "Create Room",
                WS_CHILD | WS_VISIBLE,
                20,
                244,
                110,
                30,
                ID_CREATE,
            );
            control(
                hwnd,
                "BUTTON",
                "Join Room",
                WS_CHILD | WS_VISIBLE,
                140,
                244,
                110,
                30,
                ID_JOIN,
            );
            control(
                hwnd,
                "BUTTON",
                "Leave Room",
                WS_CHILD | WS_VISIBLE,
                260,
                244,
                110,
                30,
                ID_LEAVE,
            );
            control(
                hwnd,
                "BUTTON",
                "Copy Invitation",
                WS_CHILD | WS_VISIBLE,
                380,
                244,
                150,
                30,
                ID_COPY,
            );
            label(
                hwnd,
                "Paste a private invitation from your friend",
                20,
                285,
                530,
            );
            let invitation_input = control(
                hwnd,
                "EDIT",
                "",
                WS_CHILD | WS_VISIBLE | WS_BORDER | ES_AUTOHSCROLL as u32,
                20,
                309,
                725,
                26,
                ID_INVITE,
            );
            let mute = control(
                hwnd,
                "BUTTON",
                "Microphone muted",
                WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32,
                20,
                351,
                170,
                26,
                ID_MUTE,
            );
            label(hwnd, "Overall loudness % (50–150)", 210, 353, 230);
            let loudness = control(
                hwnd,
                "EDIT",
                &preferences.loudness.to_string(),
                WS_CHILD | WS_VISIBLE | WS_BORDER | ES_NUMBER as u32,
                450,
                350,
                70,
                26,
                ID_LOUDNESS,
            );
            control(
                hwnd,
                "BUTTON",
                "Set",
                WS_CHILD | WS_VISIBLE,
                530,
                348,
                60,
                28,
                ID_VOLUME_SET,
            );
            let roblox_state = label(hwnd, "Telemetry: unavailable", 20, 381, 720);
            label(
                hwnd,
                "Fixed range: 60 studs. Invalid/stale telemetry mutes audio.",
                20,
                402,
                720,
            );
            label(hwnd, "Simulated XYZ (for tests):", 20, 422, 200);
            let positions = [ID_X, ID_Y, ID_Z].map(|id| {
                control(
                    hwnd,
                    "EDIT",
                    "0",
                    WS_CHILD | WS_VISIBLE | WS_BORDER | ES_AUTOHSCROLL as u32,
                    220 + (id - ID_X) as i32 * 82,
                    418,
                    75,
                    25,
                    id,
                )
            });
            control(
                hwnd,
                "BUTTON",
                "Set Position",
                WS_CHILD | WS_VISIBLE,
                478,
                416,
                110,
                28,
                ID_POSITION,
            );
            control(
                hwnd,
                "BUTTON",
                "Unavailable",
                WS_CHILD | WS_VISIBLE,
                598,
                416,
                110,
                28,
                ID_UNAVAILABLE,
            );
            label(hwnd, "Participants", 20, 458, 350);
            let participants_view = control(
                hwnd,
                "EDIT",
                "None",
                WS_CHILD
                    | WS_VISIBLE
                    | WS_BORDER
                    | ES_MULTILINE as u32
                    | ES_READONLY as u32
                    | WS_VSCROLL,
                20,
                483,
                725,
                65,
                0,
            );
            label(hwnd, "Connection details", 20, 558, 350);
            let log_view = control(
                hwnd,
                "EDIT",
                "",
                WS_CHILD
                    | WS_VISIBLE
                    | WS_BORDER
                    | ES_MULTILINE as u32
                    | ES_READONLY as u32
                    | WS_VSCROLL,
                20,
                582,
                725,
                90,
                0,
            );
            let update_status = label(hwnd, "Updates: checking…", 20, 682, 550);
            control(
                hwnd,
                "BUTTON",
                "Notes",
                WS_CHILD | WS_VISIBLE,
                585,
                678,
                75,
                28,
                ID_UPDATE_NOTES,
            );
            control(
                hwnd,
                "BUTTON",
                "Install",
                WS_CHILD | WS_VISIBLE,
                668,
                678,
                80,
                28,
                ID_UPDATE_NOW,
            );
            Self {
                hwnd,
                actions,
                events,
                preferences,
                username,
                experimental,
                acknowledged,
                invitation_input,
                invitation: String::new(),
                mute,
                loudness,
                positions,
                tailnet,
                status,
                roblox_state,
                participants_view,
                log_view,
                logs: Vec::new(),
                participants: HashSet::new(),
                connected: false,
                hosting: false,
                room_mode: RoomMode::Simulated,
                telemetry_valid: false,
                update_status,
                update_offer: None,
                update_due: None,
                update_notes: String::new(),
                mandatory_update: false,
                updating: false,
            }
        }
    }

    fn record(&mut self, message: String) {
        if let Some(id) = message.strip_prefix("WolfLink: Voice: Voice participant ") {
            if let Some(id) = id.strip_suffix(" joined") {
                self.participants.insert(id.into());
            }
            if let Some(id) = id.strip_suffix(" left") {
                self.participants.remove(id);
            }
        }
        self.logs.push(message);
        if self.logs.len() > 12 {
            self.logs.remove(0);
        }
        set_text(self.log_view, &self.logs.join("\r\n"));
        set_text(
            self.participants_view,
            &if self.participants.is_empty() {
                "None".into()
            } else {
                self.participants
                    .iter()
                    .map(|id| format!("Discord user {id}"))
                    .collect::<Vec<_>>()
                    .join("\r\n")
            },
        );
    }

    fn poll(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::Tailnet(result) => set_text(
                    self.tailnet,
                    &format!("Tailscale: {}", result.unwrap_or_else(|error| error)),
                ),
                Event::Roblox(running) => {
                    if (!self.connected || self.room_mode == RoomMode::ExperimentalRoblox)
                        && !self.telemetry_valid
                        && !text(self.roblox_state).starts_with("Roblox discovery failed:")
                        && !text(self.roblox_state)
                            .starts_with("Read-only Roblox provider: unsupported")
                    {
                        set_text(
                            self.roblox_state,
                            if running {
                                "Real Roblox telemetry: unavailable (Roblox detected)"
                            } else {
                                "Real Roblox telemetry: unavailable (Roblox not detected)"
                            },
                        );
                    }
                }
                Event::Started {
                    invitation,
                    host,
                    mode,
                } => {
                    self.invitation = invitation;
                    self.hosting = host;
                    self.connected = true;
                    self.room_mode = mode;
                    self.telemetry_valid = false;
                    if mode == RoomMode::ExperimentalRoblox {
                        set_text(
                            self.roblox_state,
                            "Real Roblox telemetry: discovering local player",
                        );
                    } else {
                        set_text(
                            self.roblox_state,
                            "Simulated telemetry: unavailable; set XYZ",
                        );
                    }
                    unsafe {
                        SendMessageW(
                            self.experimental,
                            BM_SETCHECK,
                            if mode == RoomMode::ExperimentalRoblox {
                                1
                            } else {
                                0
                            },
                            0,
                        );
                    }
                    set_text(
                        self.status,
                        if host {
                            "Relay hosting; Discord voice starting. Host must remain online."
                        } else {
                            "Joining invited host; Discord voice starting."
                        },
                    );
                }
                Event::Stopped => {
                    self.connected = false;
                    self.hosting = false;
                    self.telemetry_valid = false;
                    self.participants.clear();
                    set_text(self.roblox_state, "Telemetry: unavailable (no room)");
                    if text(self.status).starts_with("Leaving room") {
                        set_text(self.status, "Not connected");
                    }
                    set_text(self.participants_view, "None");
                    if self.update_offer.is_some() && !self.updating {
                        self.update_due = Some(Instant::now() + Duration::from_secs(5));
                    }
                }
                Event::Line(line) => {
                    if line.contains("Call status: Connected") {
                        set_text(self.status, "Discord voice connected");
                    } else if line.contains("Relay authenticated Discord user") {
                        set_text(self.status, "Discord authenticated; relay connected");
                    } else if line.starts_with("WolfLink: Relay disconnected")
                        || line.starts_with("WolfLink: Relay connection failed")
                        || line.starts_with("WolfLink: Invited host")
                    {
                        set_text(self.status, &line);
                    }
                    if let Some(name) = line.strip_prefix("WolfLink: Roblox player: ") {
                        self.telemetry_valid = true;
                        set_text(
                            self.roblox_state,
                            &format!(
                                "Read-only Roblox provider: {name} verified; XYZ valid (Unverified Server)"
                            ),
                        );
                    } else if line.contains("Roblox telemetry unavailable") {
                        self.telemetry_valid = false;
                        set_text(
                            self.roblox_state,
                            "Real Roblox telemetry: unavailable; voice fails closed",
                        );
                    } else if let Some(reason) =
                        line.strip_prefix("WolfLink: Roblox reader: Roblox discovery failed: ")
                    {
                        self.telemetry_valid = false;
                        let unsupported = reason.contains("does not match the validated");
                        let message = if unsupported {
                            "Read-only Roblox provider: unsupported client; XYZ unavailable"
                                .to_string()
                        } else {
                            format!(
                                "Roblox discovery failed: {}",
                                reason.chars().take(90).collect::<String>()
                            )
                        };
                        set_text(self.roblox_state, &message);
                    }
                    self.record(line);
                }
                Event::Error(error) => {
                    set_text(self.status, &format!("Error: {error}"));
                    self.record(format!("Error: {error}"));
                }
                Event::UpdateCurrent => {
                    if self.update_offer.is_none() && !self.mandatory_update {
                        set_text(self.update_status, "Updates: current");
                    }
                }
                Event::UpdateAvailable(offer) => {
                    if let CheckResult::Available {
                        manifest,
                        notes,
                        mandatory,
                        ..
                    } = &offer
                    {
                        self.mandatory_update = *mandatory;
                        self.update_notes = notes.clone();
                        set_text(
                            self.update_status,
                            &format!(
                                "Update {} {}",
                                manifest.version,
                                if *mandatory { "required" } else { "available" },
                            ),
                        );
                        self.update_due = Some(Instant::now() + Duration::from_secs(10));
                        self.update_offer = Some(offer);
                    }
                }
                Event::UpdateStatus(message) => {
                    if message.starts_with("Update failed safely:") {
                        self.updating = false;
                        self.update_due = Some(Instant::now() + Duration::from_secs(30 * 60));
                    }
                    set_text(self.update_status, &format!("Updates: {message}"));
                    if message.starts_with("Update failed safely:") {
                        self.record(message);
                    }
                }
                Event::UpdateRestarting => {
                    set_text(self.update_status, "Updates: verified; restarting…");
                    unsafe { PostMessageW(self.hwnd, WM_CLOSE, 0, 0) };
                }
                Event::UpdatePolicyBlocked(reason) => {
                    self.mandatory_update = true;
                    set_text(self.update_status, &format!("Updates: {reason}"));
                    self.record(format!("Update policy blocks new rooms: {reason}"));
                }
            }
        }
        if !self.connected
            && !self.updating
            && self.update_due.is_some_and(|due| Instant::now() >= due)
        {
            self.start_update();
        }
    }

    fn start_update(&mut self) {
        if self.connected {
            self.update_due = None;
            set_text(self.update_status, "Updates: deferred until room ends");
            return;
        }
        if let Some(offer) = self.update_offer.clone() {
            self.updating = true;
            self.update_due = None;
            set_text(self.update_status, "Updates: downloading…");
            let _ = self.actions.send(Action::ApplyUpdate(offer));
        }
    }

    fn command(&mut self, hwnd: HWND, id: u16) {
        match id {
            ID_CREATE if !self.connected && self.mandatory_update => {
                self.record(
                    "A signed mandatory update must install before joining a new room".into(),
                );
            }
            ID_JOIN if !self.connected && self.mandatory_update => {
                self.record(
                    "A signed mandatory update must install before joining a new room".into(),
                );
            }
            ID_CREATE if !self.connected => {
                self.preferences.roblox_username = text(self.username);
                let mode = if checked(self.experimental) {
                    RoomMode::ExperimentalRoblox
                } else {
                    RoomMode::Simulated
                };
                let acknowledged = checked(self.acknowledged);
                set_text(self.status, "Starting private room…");
                let _ = self.actions.send(Action::Create {
                    mode,
                    username: self.preferences.roblox_username.clone(),
                    loudness: self.preferences.loudness,
                    acknowledged,
                });
            }
            ID_JOIN if !self.connected => {
                self.preferences.roblox_username = text(self.username);
                set_text(self.status, "Joining private room…");
                let _ = self.actions.send(Action::Join {
                    invitation: text(self.invitation_input).trim().into(),
                    username: self.preferences.roblox_username.clone(),
                    loudness: self.preferences.loudness,
                    acknowledged: checked(self.acknowledged),
                });
            }
            ID_LEAVE => {
                set_text(self.status, "Leaving room…");
                let _ = self.actions.send(Action::Stop);
            }
            ID_COPY if self.hosting => {
                if let Err(error) = clipboard(hwnd, &self.invitation) {
                    self.record(format!("Error: {error}"));
                } else {
                    self.record(
                        "Invitation copied. Share privately with a permitted Tailscale device."
                            .into(),
                    );
                }
            }
            ID_MUTE => {
                let _ = self.actions.send(Action::Control(format!(
                    "mute {}",
                    if checked(self.mute) { "on" } else { "off" }
                )));
            }
            ID_VOLUME_SET => {
                if let Ok(value) = text(self.loudness).parse::<u16>()
                    && (50..=150).contains(&value)
                {
                    self.preferences.loudness = value;
                    let _ = self
                        .actions
                        .send(Action::Control(format!("loudness {value}")));
                    self.record(format!("Overall loudness: {value}%"));
                } else {
                    self.record("Loudness must be 50–150%".into());
                }
            }
            ID_POSITION if self.connected && self.room_mode == RoomMode::Simulated => {
                let values = self.positions.map(text);
                if values.iter().all(|v| {
                    v.parse::<f32>()
                        .is_ok_and(|n| n.is_finite() && n.abs() <= 1_000_000.0)
                }) {
                    let _ = self.actions.send(Action::Control(format!(
                        "position {} {} {}",
                        values[0], values[1], values[2]
                    )));
                    set_text(self.roblox_state, "Simulated telemetry: position submitted");
                } else {
                    self.record("Invalid simulated coordinates".into());
                }
            }
            ID_UNAVAILABLE if self.connected && self.room_mode == RoomMode::Simulated => {
                let _ = self.actions.send(Action::Control("unavailable".into()));
                set_text(
                    self.roblox_state,
                    "Simulated telemetry: unavailable; voice fails closed",
                );
            }
            ID_UPDATE_NOTES => {
                let notes = if self.update_notes.is_empty() {
                    "No update release notes are available yet."
                } else {
                    &self.update_notes
                };
                unsafe {
                    MessageBoxW(
                        hwnd,
                        wide(notes).as_ptr(),
                        wide("WolfLink update notes").as_ptr(),
                        MB_OK | MB_ICONINFORMATION,
                    );
                }
            }
            ID_UPDATE_NOW => self.start_update(),
            _ => {}
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_COMMAND => {
            let id = (wparam & 0xffff) as u16;
            STATE.with(|state| {
                if let Some(ui) = state.borrow_mut().as_mut() {
                    ui.command(hwnd, id);
                }
            });
            0
        }
        WM_TIMER => {
            STATE.with(|state| {
                if let Some(ui) = state.borrow_mut().as_mut() {
                    ui.poll();
                }
            });
            0
        }
        WM_DESTROY => {
            STATE.with(|state| {
                if let Some(mut ui) = state.borrow_mut().take() {
                    ui.preferences.roblox_username = text(ui.username);
                    save_preferences(&ui.preferences);
                    let _ = ui.actions.send(Action::Exit);
                }
            });
            unsafe {
                PostQuitMessage(0);
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

pub fn run() {
    unsafe {
        let instance = GetModuleHandleW(null());
        let class_name = wide("WolfLinkWindow");
        let title = wide("WolfLink Proximity");
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class_name.as_ptr(),
            hbrBackground: (COLOR_WINDOW as usize + 1) as *mut c_void,
            ..std::mem::zeroed()
        };
        if RegisterClassW(&class) == 0 {
            return;
        }
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            790,
            770,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            return;
        }
        STATE.with(|state| *state.borrow_mut() = Some(Ui::new(hwnd)));
        SetTimer(hwnd, 1, 250, None);
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
            if IsDialogMessageW(hwnd, &message) == 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
}
