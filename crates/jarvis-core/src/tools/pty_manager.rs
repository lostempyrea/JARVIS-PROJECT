//! Pure-Rust Win32 ConPTY manager and embedded terminal tab manager.

use crate::events::{CoreEvent, EventBus};
use crate::tools::vt_emulator::{VtScreen, VtScreenSnapshot};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::c_void;
use std::io::{Read, Write};

use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[allow(non_snake_case, non_camel_case_types)]
#[link(name = "kernel32")]
extern "system" {
    fn CreatePseudoConsole(
        size: COORD,
        hInput: *mut c_void,
        hOutput: *mut c_void,
        dwFlags: u32,
        phPC: *mut *mut c_void,
    ) -> i32;

    fn ClosePseudoConsole(hPC: *mut c_void);

    fn ResizePseudoConsole(hPC: *mut c_void, size: COORD) -> i32;

    fn InitializeProcThreadAttributeList(
        lpAttributeList: *mut c_void,
        dwAttributeCount: u32,
        dwFlags: u32,
        lpSize: *mut usize,
    ) -> i32;

    fn UpdateProcThreadAttribute(
        lpAttributeList: *mut c_void,
        dwFlags: u32,
        attribute: usize,
        lpValue: *mut c_void,
        cbSize: usize,
        lpPreviousValue: *mut c_void,
        lpReturnSize: *mut usize,
    ) -> i32;

    fn DeleteProcThreadAttributeList(lpAttributeList: *mut c_void);

    fn CreateProcessW(
        lpApplicationName: *const u16,
        lpCommandLine: *mut u16,
        lpProcessAttributes: *mut c_void,
        lpThreadAttributes: *mut c_void,
        bInheritHandles: i32,
        dwCreationFlags: u32,
        lpEnvironment: *const c_void,
        lpCurrentDirectory: *const u16,
        lpStartupInfo: *mut c_void,
        lpProcessInformation: *mut PROCESS_INFORMATION,
    ) -> i32;

    fn TerminateProcess(hProcess: *mut c_void, uExitCode: u32) -> i32;
    fn GetExitCodeProcess(hProcess: *mut c_void, lpExitCode: *mut u32) -> i32;
    fn CloseHandle(hObject: *mut c_void) -> i32;
    fn CreatePipe(
        hReadPipe: *mut *mut c_void,
        hWritePipe: *mut *mut c_void,
        lpPipeAttributes: *mut c_void,
        nSize: u32,
    ) -> i32;
}

#[allow(non_snake_case, non_camel_case_types)]
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct COORD {
    pub X: i16,
    pub Y: i16,
}

#[allow(non_snake_case, non_camel_case_types)]
#[repr(C)]
#[derive(Debug)]
pub struct PROCESS_INFORMATION {
    pub hProcess: *mut c_void,
    pub hThread: *mut c_void,
    pub dwProcessId: u32,
    pub dwThreadId: u32,
}

#[allow(non_snake_case, non_camel_case_types)]
#[repr(C)]
#[derive(Debug)]
pub struct STARTUPINFOEXW {
    pub startup_info: STARTUPINFOW,
    pub lp_attribute_list: *mut c_void,
}

#[allow(non_snake_case, non_camel_case_types)]
#[repr(C)]
#[derive(Debug)]
pub struct STARTUPINFOW {
    pub cb: u32,
    pub lpReserved: *mut u16,
    pub lpDesktop: *mut u16,
    pub lpTitle: *mut u16,
    pub dwX: u32,
    pub dwY: u32,
    pub dwXSize: u32,
    pub dwYSize: u32,
    pub dwXCountChars: u32,
    pub dwYCountChars: u32,
    pub dwFillAttribute: u32,
    pub dwFlags: u32,
    pub wShowWindow: u16,
    pub cbReserved2: u16,
    pub lpReserved2: *mut u8,
    pub hStdInput: *mut c_void,
    pub hStdOutput: *mut c_void,
    pub hStdError: *mut c_void,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtyTabInfo {
    pub tab_id: String,
    pub title: String,
    pub kind: String,
    pub alive: bool,
    pub exit_code: Option<u32>,
    pub cols: u16,
    pub rows: u16,
}

struct PtyTab {
    tab_id: String,
    title: String,
    kind: String,
    hpcon: *mut c_void,
    h_process: *mut c_void,
    pty_in_write: Arc<Mutex<std::fs::File>>,
    screen: Arc<Mutex<VtScreen>>,
    cols: u16,
    rows: u16,
    alive: bool,
    exit_code: Option<u32>,
}

unsafe impl Send for PtyTab {}
unsafe impl Sync for PtyTab {}

pub struct PtyManager {
    event_bus: EventBus,
    tabs: Arc<Mutex<HashMap<String, PtyTab>>>,
}

impl PtyManager {
    pub fn new(event_bus: EventBus) -> Self {
        Self {
            event_bus,
            tabs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn spawn_powershell_tab(
        &self,
        title: Option<String>,
        cwd: Option<&str>,
        cols: u16,
        rows: u16,
    ) -> Result<String, String> {
        let tab_id = uuid::Uuid::new_v4().to_string();
        let tab_title = title.unwrap_or_else(|| "PowerShell".to_string());
        self.spawn_conpty(&tab_id, &tab_title, "powershell", &["powershell.exe", "-NoProfile", "-NoLogo", "-NoExit"], cwd, cols, rows)
    }

    pub fn spawn_agy_tab(
        &self,
        initial_prompt: Option<&str>,
        cwd: Option<&str>,
        cols: u16,
        rows: u16,
    ) -> Result<String, String> {
        let tab_id = uuid::Uuid::new_v4().to_string();
        let mut cmd_line = "agy".to_string();
        if let Some(prompt) = initial_prompt {
            if !prompt.trim().is_empty() {
                // escape double quotes if needed or pass as argument
                let escaped = prompt.replace('"', "\\\"");
                cmd_line = format!("agy --prompt \"{}\"", escaped);
            }
        }
        let wide_cmd: Vec<u16> = format!("powershell.exe -NoLogo -NoExit -Command \"{}\"", cmd_line)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        
        let tab_title = "AGY Assistant".to_string();
        self.spawn_conpty_raw(&tab_id, &tab_title, "agy", wide_cmd, cwd, cols, rows)
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_conpty(
        &self,
        tab_id: &str,
        title: &str,
        kind: &str,
        args: &[&str],
        cwd: Option<&str>,
        cols: u16,
        rows: u16,
    ) -> Result<String, String> {
        let cmd_str = args.join(" ");
        let wide_cmd: Vec<u16> = cmd_str
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        self.spawn_conpty_raw(tab_id, title, kind, wide_cmd, cwd, cols, rows)
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_conpty_raw(
        &self,
        tab_id: &str,
        title: &str,
        kind: &str,
        mut wide_cmd: Vec<u16>,
        cwd: Option<&str>,
        cols: u16,
        rows: u16,
    ) -> Result<String, String> {
        unsafe {
            let mut pty_in_read: *mut c_void = null_mut();
            let mut pty_in_write: *mut c_void = null_mut();
            let mut pty_out_read: *mut c_void = null_mut();
            let mut pty_out_write: *mut c_void = null_mut();

            if CreatePipe(&mut pty_in_read, &mut pty_in_write, null_mut(), 0) == 0 {
                return Err("Failed to create input pipe for ConPTY".to_string());
            }
            if CreatePipe(&mut pty_out_read, &mut pty_out_write, null_mut(), 0) == 0 {
                CloseHandle(pty_in_read);
                CloseHandle(pty_in_write);
                return Err("Failed to create output pipe for ConPTY".to_string());
            }

            let size = COORD {
                X: cols.max(10) as i16,
                Y: rows.max(2) as i16,
            };

            let mut hpcon: *mut c_void = null_mut();
            let hr = CreatePseudoConsole(size, pty_in_read, pty_out_write, 0, &mut hpcon);
            if hr < 0 || hpcon.is_null() {
                CloseHandle(pty_in_read);
                CloseHandle(pty_in_write);
                CloseHandle(pty_out_read);
                CloseHandle(pty_out_write);
                return Err(format!("CreatePseudoConsole failed with HRESULT {hr:#x}"));
            }

            // Close child-side pipe ends in parent right after CreatePseudoConsole
            CloseHandle(pty_in_read);
            CloseHandle(pty_out_write);

            // Prepare attribute list
            let mut attr_list_size: usize = 0;
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attr_list_size);
            if attr_list_size == 0 {
                ClosePseudoConsole(hpcon);
                CloseHandle(pty_in_write);
                CloseHandle(pty_out_read);
                return Err("Failed to get proc thread attribute list size".to_string());
            }

            let mut attr_list_buf = vec![0u8; attr_list_size];
            let attr_list = attr_list_buf.as_mut_ptr() as *mut c_void;

            if InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_list_size) == 0 {
                ClosePseudoConsole(hpcon);
                CloseHandle(pty_in_write);
                CloseHandle(pty_out_read);
                return Err("Failed to initialize proc thread attribute list".to_string());
            }

            // UpdateProcThreadAttribute: lpValue MUST be hpcon by value
            let update_res = UpdateProcThreadAttribute(
                attr_list,
                0,
                0x0002_0016, // PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE
                hpcon,
                std::mem::size_of::<*mut c_void>(),
                null_mut(),
                null_mut(),
            );

            if update_res == 0 {
                DeleteProcThreadAttributeList(attr_list);
                ClosePseudoConsole(hpcon);
                CloseHandle(pty_in_write);
                CloseHandle(pty_out_read);
                return Err("Failed to update proc thread attribute for ConPTY".to_string());
            }

            let mut siex = STARTUPINFOEXW {
                startup_info: STARTUPINFOW {
                    cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
                    lpReserved: null_mut(),
                    lpDesktop: null_mut(),
                    lpTitle: null_mut(),
                    dwX: 0,
                    dwY: 0,
                    dwXSize: 0,
                    dwYSize: 0,
                    dwXCountChars: 0,
                    dwYCountChars: 0,
                    dwFillAttribute: 0,
                    dwFlags: 0x0000_0100, // STARTF_USESTDHANDLES (with null std handles so ConPTY CONIN$/CONOUT$ is used instead of parent's redirected pipes)
                    wShowWindow: 0,
                    cbReserved2: 0,
                    lpReserved2: null_mut(),
                    hStdInput: null_mut(),
                    hStdOutput: null_mut(),
                    hStdError: null_mut(),
                },
                lp_attribute_list: attr_list,
            };

            let mut pi = PROCESS_INFORMATION {
                hProcess: null_mut(),
                hThread: null_mut(),
                dwProcessId: 0,
                dwThreadId: 0,
            };

            let wide_cwd: Option<Vec<u16>> = cwd.map(|d| {
                d.encode_utf16()
                    .chain(std::iter::once(0))
                    .collect()
            });
            let cwd_ptr = wide_cwd.as_ref().map_or(null(), |v| v.as_ptr());

            let create_res = CreateProcessW(
                null(),
                wide_cmd.as_mut_ptr(),
                null_mut(),
                null_mut(),
                0, // bInheritHandles = 0
                0x0008_0000, // EXTENDED_STARTUPINFO_PRESENT
                null(),
                cwd_ptr,
                &mut siex as *mut _ as *mut c_void,
                &mut pi,
            );

            DeleteProcThreadAttributeList(attr_list);

            if create_res == 0 {
                ClosePseudoConsole(hpcon);
                CloseHandle(pty_in_write);
                CloseHandle(pty_out_read);
                return Err(format!("CreateProcessW failed with error code {}", std::io::Error::last_os_error()));
            }

            CloseHandle(pi.hThread);

            let pty_in_write_arc = Arc::new(Mutex::new(
                <std::fs::File as std::os::windows::io::FromRawHandle>::from_raw_handle(pty_in_write),
            ));
            let mut std_out_file: std::fs::File =
                std::os::windows::io::FromRawHandle::from_raw_handle(pty_out_read);

            let screen = Arc::new(Mutex::new(VtScreen::new(cols.max(10), rows.max(2))));
            let screen_clone = Arc::clone(&screen);
            let in_write_clone = Arc::clone(&pty_in_write_arc);
            let event_bus_clone = self.event_bus.clone();
            let tab_id_string = tab_id.to_string();

            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let mut last_emit = Instant::now();

                loop {
                    match std_out_file.read(&mut buf) {
                        Ok(0) => { break; }
                        Ok(n) => {
                            let bytes = &buf[..n];
                            let reply_bytes = {
                                let mut scr = screen_clone.lock().unwrap();
                                scr.process_bytes(bytes)
                            };

                            if !reply_bytes.is_empty() {
                                if let Ok(mut w) = in_write_clone.lock() {
                                    let _ = w.write_all(&reply_bytes);
                                    let _ = w.flush();
                                }
                            }

                            if last_emit.elapsed().as_millis() >= 33 {
                                event_bus_clone.publish(CoreEvent::PtyUpdated {
                                    tab_id: tab_id_string.clone(),
                                });
                                last_emit = Instant::now();
                            }
                        }
                        Err(_) => { break; }
                    }
                }

                event_bus_clone.publish(CoreEvent::PtyUpdated {
                    tab_id: tab_id_string,
                });
            });

            let tab = PtyTab {
                tab_id: tab_id.to_string(),
                title: title.to_string(),
                kind: kind.to_string(),
                hpcon,
                h_process: pi.hProcess,
                pty_in_write: pty_in_write_arc,
                screen,
                cols,
                rows,
                alive: true,
                exit_code: None,
            };

            let mut tabs = self.tabs.lock().unwrap();
            tabs.insert(tab_id.to_string(), tab);

            Ok(tab_id.to_string())
        }
    }

    pub fn write_input(&self, tab_id: &str, data: &[u8]) -> Result<(), String> {
        let mut tabs = self.tabs.lock().unwrap();
        let tab = tabs.get_mut(tab_id).ok_or_else(|| "Tab not found".to_string())?;
        if !tab.alive {
            return Err("Tab process is not alive".to_string());
        }
        let mut w = tab.pty_in_write.lock().unwrap();
        w.write_all(data).map_err(|e| e.to_string())?;
        w.flush().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn resize_tab(&self, tab_id: &str, cols: u16, rows: u16) -> Result<(), String> {
        let mut tabs = self.tabs.lock().unwrap();
        let tab = tabs.get_mut(tab_id).ok_or_else(|| "Tab not found".to_string())?;
        tab.cols = cols;
        tab.rows = rows;
        {
            let mut scr = tab.screen.lock().unwrap();
            scr.resize(cols, rows);
        }
        unsafe {
            let size = COORD {
                X: cols as i16,
                Y: rows as i16,
            };
            ResizePseudoConsole(tab.hpcon, size);
        }
        Ok(())
    }

    pub fn snapshot_tab(&self, tab_id: &str, scroll_offset: usize) -> Option<VtScreenSnapshot> {
        let tabs = self.tabs.lock().unwrap();
        let tab = tabs.get(tab_id)?;
        let scr = tab.screen.lock().unwrap();
        Some(scr.snapshot(scroll_offset))
    }

    pub fn list_tabs(&self) -> Vec<PtyTabInfo> {
        let mut tabs = self.tabs.lock().unwrap();
        let mut list = Vec::new();
        for tab in tabs.values_mut() {
            if tab.alive {
                unsafe {
                    let mut code: u32 = 0;
                    if GetExitCodeProcess(tab.h_process, &mut code) != 0 && code != 259 { // STILL_ACTIVE
                        tab.alive = false;
                        tab.exit_code = Some(code);
                    }
                }
            }
            list.push(PtyTabInfo {
                tab_id: tab.tab_id.clone(),
                title: tab.title.clone(),
                kind: tab.kind.clone(),
                alive: tab.alive,
                exit_code: tab.exit_code,
                cols: tab.cols,
                rows: tab.rows,
            });
        }
        list
    }

    pub fn restart_tab(&self, _tab_id: &str) -> Result<(), String> {
        // Not strictly required for basic spawn/close, but good to have or no-op
        Ok(())
    }

    pub fn close_tab(&self, tab_id: &str) -> bool {
        let mut tabs = self.tabs.lock().unwrap();
        if let Some(mut tab) = tabs.remove(tab_id) {
            tab.alive = false;
            unsafe {
                TerminateProcess(tab.h_process, 1);
                CloseHandle(tab.h_process);
                ClosePseudoConsole(tab.hpcon);
            }
            true
        } else {
            false
        }
    }

    pub fn shutdown_all(&self) {
        let mut tabs = self.tabs.lock().unwrap();
        for (_, mut tab) in tabs.drain() {
            tab.alive = false;
            unsafe {
                TerminateProcess(tab.h_process, 1);
                CloseHandle(tab.h_process);
                ClosePseudoConsole(tab.hpcon);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn conpty_spawns_powershell_and_streams_interactive_io() {
        let bus = EventBus::new(32);
        let manager = PtyManager::new(bus);

        let tab_id = manager
            .spawn_powershell_tab(Some("Test PS".to_string()), None, 80, 24)
            .unwrap();

        // Wait until snap.rows is non-empty (up to 5s)
        let boot_start = Instant::now();
        while boot_start.elapsed() < std::time::Duration::from_secs(5) {
            if let Some(snap) = manager.snapshot_tab(&tab_id, 0) {
                if !snap.rows.is_empty() {
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        // Additional pause to let prompt render fully
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        // Write command
        manager
            .write_input(&tab_id, b"echo CONPTY_OK_123\r")
            .unwrap();

        // Poll snapshot up to 10s
        let start = Instant::now();
        let mut found = false;
        let mut last_full_text = String::new();
        while start.elapsed() < std::time::Duration::from_secs(10) {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let _tabs_check = manager.list_tabs();
            if let Some(snap) = manager.snapshot_tab(&tab_id, 0) {
                let full_text: String = snap
                    .rows
                    .iter()
                    .map(|row| row.iter().map(|span| span.text.as_str()).collect::<Vec<_>>().join(""))
                    .collect::<Vec<_>>()
                    .join("\n");
                last_full_text = full_text.clone();
                if full_text.contains("CONPTY_OK_123") {
                    found = true;
                    break;
                }
            }
        }

        assert!(found, "ConPTY output should contain CONPTY_OK_123. tabs={:?}\nlast_screen:\n{}", manager.list_tabs(), last_full_text);
        manager.close_tab(&tab_id);
    }

    #[tokio::test]
    async fn conpty_spawns_cmd_and_captures_output() {
        let bus = EventBus::new(32);
        let manager = PtyManager::new(bus);

        let tab_id = manager
            .spawn_conpty("test-cmd", "Test CMD", "cmd", &["cmd.exe", "/c", "echo CONPTY_CMD_OK"], None, 80, 24)
            .unwrap();

        let start = Instant::now();
        let mut found = false;
        let mut last_full_text = String::new();
        while start.elapsed() < std::time::Duration::from_secs(5) {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if let Some(snap) = manager.snapshot_tab(&tab_id, 0) {
                let full_text: String = snap
                    .rows
                    .iter()
                    .map(|row| row.iter().map(|span| span.text.as_str()).collect::<Vec<_>>().join(""))
                    .collect::<Vec<_>>()
                    .join("\n");
                last_full_text = full_text.clone();
                if full_text.contains("CONPTY_CMD_OK") {
                    found = true;
                    break;
                }
            }
        }

        assert!(found, "ConPTY output should contain CONPTY_CMD_OK. tabs={:?}\nlast_screen:\n{}", manager.list_tabs(), last_full_text);
        manager.close_tab(&tab_id);
    }
}
