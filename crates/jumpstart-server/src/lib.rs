//! Shared CLI code for Jumpstart's daemon administration and Worlds entry points.
//!
//! Both binaries use the same report formatter. On Linux they also share the
//! credential-checked socket client, so Worlds never opens server storage.

use std::{error::Error, io::Write};

#[cfg(target_os = "linux")]
pub mod local_admin;
#[cfg(target_os = "linux")]
pub mod mass_client;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod remote_mass;

/// Default administrative socket in the private systemd runtime directory.
pub const SOCKET_PATH: &str = "/run/jumpstart/admin.sock";

/// Write a metadata, migration, snapshot, or persisted-job report and flush output.
///
/// # Arguments
///
/// * `report` — borrowed `&serde_json::Value` containing the serialized report.
/// * `title` — `&str` heading describing the verified report's scope in text mode.
/// * `json` — `bool` selecting pretty JSON when true, or labeled text when false.
///
/// # Returns
///
/// `Result<(), Box<dyn Error>>`, with `Ok(())` after a successful flush.
///
/// # Errors
///
/// Propagates serialization and stdout write/flush errors. Output may already
/// contain part of the report when an I/O error occurs.
pub fn print_report(
    report: &serde_json::Value,
    title: &str,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let mut output = std::io::stdout().lock();
    if json {
        writeln!(output, "{}", serde_json::to_string_pretty(report)?)?;
    } else {
        writeln!(output, "{title}")?;
        if let serde_json::Value::Object(fields) = report {
            print_fields(&mut output, fields, 2)?;
        }
    }
    output.flush()?;
    Ok(())
}

/// Print nested report objects as indented labels for people reading the terminal.
///
/// For example, capacity thresholds appear under `assessment` on separate lines
/// instead of being squeezed into a JSON object inside the text report.
///
/// # Arguments
///
/// * `output` — mutable borrowed writer receiving the report's text fields.
/// * `fields` — borrowed JSON object produced from a typed local storage report.
/// * `indent` — `usize` number of leading spaces for fields at this object level.
///
/// # Returns
///
/// `std::io::Result<()>` after writing this object and its nested objects.
///
/// # Errors
///
/// Propagates write failures; output may already contain part of the report.
fn print_fields(
    output: &mut impl Write,
    fields: &serde_json::Map<String, serde_json::Value>,
    indent: usize,
) -> std::io::Result<()> {
    for (name, value) in fields {
        write!(output, "{blank:indent$}{name}:", blank = "")?;
        match value {
            serde_json::Value::Object(nested) => {
                writeln!(output)?;
                print_fields(output, nested, indent + 2)?;
            }
            serde_json::Value::String(text) => writeln!(output, " {text}")?,
            other => writeln!(output, " {other}")?,
        }
    }
    Ok(())
}
