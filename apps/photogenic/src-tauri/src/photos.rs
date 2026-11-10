//! Copies bounded JSON into the native PhotoKit adapter and returns its reply.
//! The FFI is confined here so the rest of the client can use a safe command.

#![allow(unsafe_code)]

use std::{ffi::c_void, slice};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;

type NativeReply = Result<Value, String>;
const MAX_RESPONSE: usize = 1024 * 1024;

/// Only these operations can cross the WebView-to-PhotoKit boundary.
#[derive(Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Permission,
    Settings,
    Page { filter: Filter, offset: u32 },
    Thumbnail { id: String },
    Details { id: String },
    Prepare { id: String },
    Queue,
    Remove { id: String },
}

/// Available native Photos query filters.
#[derive(Deserialize, Serialize)]
pub enum Filter {
    All,
    Photos,
    Videos,
}

unsafe extern "C" {
    fn photogenic_photos(
        request: *const u8,
        length: usize,
        context: *mut c_void,
        reply: extern "C" fn(*mut c_void, *const u8, usize, bool),
    );
}

/// Start one Apple Photos command without blocking the Rust or Apple UI thread.
#[tauri::command]
pub async fn photos(request: Request) -> NativeReply {
    let bytes = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
    if bytes.len() > 4096 {
        return Err("The Photos request exceeded its limit.".into());
    }
    let (send, receive) = oneshot::channel::<NativeReply>();
    let context = Box::into_raw(Box::new(send)).cast();
    // Swift copies the request before this call returns and calls complete once.
    unsafe { photogenic_photos(bytes.as_ptr(), bytes.len(), context, complete) };
    receive
        .await
        .map_err(|_| "The Photos adapter stopped.".to_string())?
}

/// Consume the callback context and copy the Swift-owned reply before returning.
extern "C" fn complete(context: *mut c_void, bytes: *const u8, length: usize, success: bool) {
    // The context is allocated above and transferred to this one native callback.
    let sender = unsafe { Box::from_raw(context.cast::<oneshot::Sender<NativeReply>>()) };
    let response = if length == 0 || length > MAX_RESPONSE || bytes.is_null() {
        Err("The Photos adapter returned an invalid response.".into())
    } else {
        // Swift keeps the byte buffer valid for the duration of this callback.
        let data = unsafe { slice::from_raw_parts(bytes, length) };
        if success {
            serde_json::from_slice(data).map_err(|error| error.to_string())
        } else {
            Err(String::from_utf8_lossy(data).into_owned())
        }
    };
    let _ = sender.send(response);
}
