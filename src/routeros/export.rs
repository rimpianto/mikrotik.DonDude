//! Turning raw `/export` output into a stable, diffable `.rsc` file.
//!
//! # Why this module exists
//!
//! A raw RouterOS export is not byte-stable across runs. Its first line is a
//! banner carrying the *current clock*:
//!
//! ```text
//! # 2024-01-15 10:22:31 by RouterOS 7.13.2
//! # software id = ABCD-EFGH
//! #
//! # model = RB5009UG+S+
//! # serial number = HGT08XXXXX
//! ```
//!
//! Committed as-is, every device would produce a diff on every run and the
//! history would say nothing about what actually changed. So the banner is
//! parsed for the facts worth keeping (version, model, serial, software id),
//! then rewritten without the timestamp. The capture time is not lost — it goes
//! into the commit, where it belongs.
//!
//! Firmware version *is* kept in the file: an upgrade is a real change and
//! showing it in the diff is the point.

use std::collections::BTreeMap;

use crate::config::Export;

/// Facts about a device, merged from the export banner and `/system` prints.
///
/// Every field is optional: metadata collection is best-effort, and a missing
/// board name must never cost us the backup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouterInfo {
    /// `/system identity` name, as configured on the device.
    pub identity: Option<String>,
    /// RouterOS version, e.g. `7.13.2`.
    pub version: Option<String>,
    /// Board / model name, e.g. `RB5009UG+S+`.
    pub model: Option<String>,
    pub serial: Option<String>,
    pub software_id: Option<String>,
    pub architecture: Option<String>,
}

impl RouterInfo {
    /// Fill empty fields from `other`, keeping values already set.
    ///
    /// Callers merge in precedence order — `/system` prints first, then the
    /// export banner as a fallback — so the more authoritative source wins.
    pub fn merge_from(&mut self, other: RouterInfo) {
        fill(&mut self.identity, other.identity);
        fill(&mut self.version, other.version);
        fill(&mut self.model, other.model);
        fill(&mut self.serial, other.serial);
        fill(&mut self.software_id, other.software_id);
        fill(&mut self.architecture, other.architecture);
    }

    /// Short human description used in commit subjects and CLI output.
    pub fn describe(&self) -> String {
        match (&self.model, &self.version) {
            (Some(model), Some(version)) => format!("{model}, RouterOS {version}"),
            (Some(model), None) => model.clone(),
            (None, Some(version)) => format!("RouterOS {version}"),
            (None, None) => "unknown model".to_string(),
        }
    }

    /// Parse the leading comment banner of an export.
    pub fn from_export_banner(raw: &str) -> Self {
        let mut info = Self::default();
        for line in banner_lines(raw) {
            let comment = line.trim_start_matches('#').trim();
            if comment.is_empty() {
                continue;
            }
            // "2024-01-15 10:22:31 by RouterOS 7.13.2"
            if let Some((_, version)) = comment.split_once(" by RouterOS ") {
                info.version = non_empty(version);
                continue;
            }
            if let Some((key, value)) = comment.split_once('=') {
                let value = non_empty(value);
                match key.trim() {
                    "model" => info.model = value,
                    "serial number" => info.serial = value,
                    "software id" => info.software_id = value,
                    _ => {}
                }
            }
        }
        info
    }

    /// Parse `/system resource print` output.
    pub fn from_resource_print(raw: &str) -> Self {
        let fields = parse_print(raw);
        Self {
            version: fields.get("version").and_then(|v| non_empty(v)),
            model: fields.get("board-name").and_then(|v| non_empty(v)),
            architecture: fields.get("architecture-name").and_then(|v| non_empty(v)),
            ..Self::default()
        }
    }

    /// Parse `/system identity print` output.
    pub fn from_identity_print(raw: &str) -> Self {
        Self {
            identity: parse_print(raw).get("name").and_then(|v| non_empty(v)),
            ..Self::default()
        }
    }

    /// Parse `/system routerboard print` output (serial number lives here).
    pub fn from_routerboard_print(raw: &str) -> Self {
        let fields = parse_print(raw);
        Self {
            serial: fields.get("serial-number").and_then(|v| non_empty(v)),
            model: fields.get("model").and_then(|v| non_empty(v)),
            ..Self::default()
        }
    }
}

/// A `/export` capture, normalized and ready to write into the backup repo.
#[derive(Debug, Clone)]
pub struct ExportedConfig {
    /// File contents to commit.
    pub contents: String,
    /// Facts recovered while capturing it.
    pub info: RouterInfo,
    /// The exact command that produced it.
    pub command: String,
}

/// Normalize raw export output for storage.
///
/// `device_name` is stamped into the rewritten banner so an `.rsc` file is
/// self-describing once it is out of the repo.
pub fn normalize(raw: &str, command: &str, device_name: &str, options: &Export) -> ExportedConfig {
    let (prefix, export_part) = split_at_export_banner(raw);
    let info = RouterInfo::from_export_banner(&export_part);
    let contents = render(
        raw,
        &prefix,
        &export_part,
        command,
        device_name,
        &info,
        options,
    );
    ExportedConfig {
        contents,
        info,
        command: command.to_string(),
    }
}

/// Re-render with a fuller [`RouterInfo`] than the banner alone could provide.
///
/// `prefix` and `export_part` come from [`split_at_export_banner`]; both are
/// derived from `raw`, which is what the verbatim mode falls back to.
pub fn render(
    raw: &str,
    prefix: &str,
    export_part: &str,
    command: &str,
    device_name: &str,
    info: &RouterInfo,
    options: &Export,
) -> String {
    if !options.normalize_header {
        // Verbatim mode: line endings only. Accepts a diff on every run in
        // exchange for a byte-faithful copy of what the device sent.
        return ensure_trailing_newline(&normalize_newlines(raw));
    }

    // The `/user` print output ahead of the export banner is live state, not
    // config, and there is nothing volatile in it to normalize. Each line is
    // prefixed with `# REM ` so the captured state stays readable in the
    // versioned file yet is ignored by RouterOS if the `.rsc` is ever pasted
    // or imported back onto a device. Empty captures (no prefix) add nothing.
    let prefix = ensure_trailing_newline(&normalize_newlines(prefix));
    let prefix = comment_as_rem(&prefix);
    let body = strip_banner(export_part);

    let mut out = String::with_capacity(body.len() + 256);
    out.push_str("# RouterOS configuration export captured by DonDude\n");
    push_field(&mut out, "device", Some(device_name));
    push_field(&mut out, "identity", info.identity.as_deref());
    push_field(&mut out, "model", info.model.as_deref());
    push_field(&mut out, "serial number", info.serial.as_deref());
    push_field(&mut out, "software id", info.software_id.as_deref());
    push_field(&mut out, "architecture", info.architecture.as_deref());
    push_field(&mut out, "routeros", info.version.as_deref());
    push_field(&mut out, "command", Some(command));
    out.push_str("#\n");
    // Deliberately absent: the capture timestamp. It changes every run and
    // would defeat change detection; the commit records it instead.
    out.push_str(&prefix);
    out.push_str(&body);
    ensure_trailing_newline(&out)
}

/// Prefix every non-empty line with `# REM `, so captured live state reads
/// as a comment block inside the `.rsc`. Blank lines pass through unchanged
/// to keep the block visually intact.
fn comment_as_rem(block: &str) -> String {
    block
        .lines()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                format!("# REM {line}")
            }
        })
        .fold(String::new(), |mut out, line| {
            out.push_str(&line);
            out.push('\n');
            out
        })
}

/// Split combined command output at the RouterOS export banner.
///
/// The executed command is `/user print detail; /user ssh-keys print; /export
/// …`, so the raw output starts with the user print output and the export
/// banner appears mid-stream. The banner is the first `#`-comment line reading
/// `# <date> by RouterOS <ver>`; that line and everything after it belong to
/// the export, everything before it to the prints.
///
/// Captures without a recognized banner (older RouterOS, a manual run) keep
/// everything in `export_part`, matching the pre-combined-command behaviour.
///
/// Returns owned strings because the split point is computed over newline-
/// normalized text, which is a fresh allocation.
pub fn split_at_export_banner(raw: &str) -> (String, String) {
    let normalized = normalize_newlines(raw);
    let mut offset = 0usize;
    for (line, end) in normalized.split_inclusive('\n').map(|l| {
        let end = l.len();
        (l.trim_end_matches('\n'), end)
    }) {
        if line.trim_start().starts_with('#')
            && line
                .trim_start_matches('#')
                .trim()
                .contains(" by RouterOS ")
        {
            return (
                normalized[..offset].to_string(),
                normalized[offset..].to_string(),
            );
        }
        offset += end;
    }
    (String::new(), normalized)
}

/// The leading run of comment/blank lines that RouterOS prepends to an export.
fn banner_lines(raw: &str) -> impl Iterator<Item = &str> {
    raw.lines()
        .take_while(|line| is_banner_line(line))
        .filter(|line| line.trim_start().starts_with('#'))
}

fn is_banner_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty() || trimmed.starts_with('#')
}

/// Drop the banner, keeping the configuration itself.
///
/// Only the *leading contiguous* comment block is removed, so `#` characters
/// inside script bodies or comment properties further down are untouched.
fn strip_banner(raw: &str) -> String {
    let normalized = normalize_newlines(raw);
    let body: String = normalized
        .lines()
        .skip_while(|line| is_banner_line(line))
        .collect::<Vec<_>>()
        .join("\n");
    body
}

/// RouterOS speaks LF over `exec` but CRLF when a PTY is involved.
fn normalize_newlines(raw: &str) -> String {
    raw.replace("\r\n", "\n").replace('\r', "\n")
}

fn ensure_trailing_newline(text: &str) -> String {
    let trimmed = text.trim_end_matches('\n');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

fn push_field(out: &mut String, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        let value = value.trim();
        if !value.is_empty() {
            out.push_str(&format!("# {key} = {value}\n"));
        }
    }
}

/// Parse RouterOS `print` output (`  key: value` lines) into a map.
///
/// Values may themselves contain `:` (build times, MAC addresses), so only the
/// first separator counts. Keys RouterOS wraps across lines are ignored rather
/// than mis-parsed — everything read here is optional metadata.
fn parse_print(raw: &str) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    for line in normalize_newlines(raw).lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            continue;
        }
        fields.insert(key.to_lowercase(), value.trim().to_string());
    }
    fields
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn fill(slot: &mut Option<String>, value: Option<String>) {
    if slot.is_none() {
        *slot = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ExportMode;

    const V7_EXPORT: &str = "\
# 2024-01-15 10:22:31 by RouterOS 7.13.2
# software id = ABCD-EFGH
#
# model = RB5009UG+S+
# serial number = HGT08XXXXX

/interface bridge
add admin-mac=48:A9:8A:00:00:01 auto-mac=no comment=defconf name=bridge
/ip address
add address=10.0.0.1/24 interface=bridge network=10.0.0.0
";

    const V6_EXPORT: &str = "\
# jan/15/2024 10:22:31 by RouterOS 6.49.10
# software id = WXYZ-1234
#
# model = RouterBOARD 750G r3
/ip service
set telnet disabled=yes
";

    fn options() -> Export {
        Export {
            mode: ExportMode::Terse,
            ..Export::default()
        }
    }

    #[test]
    fn parses_v7_banner() {
        let info = RouterInfo::from_export_banner(V7_EXPORT);
        assert_eq!(info.version.as_deref(), Some("7.13.2"));
        assert_eq!(info.model.as_deref(), Some("RB5009UG+S+"));
        assert_eq!(info.serial.as_deref(), Some("HGT08XXXXX"));
        assert_eq!(info.software_id.as_deref(), Some("ABCD-EFGH"));
    }

    #[test]
    fn parses_v6_banner_with_legacy_date_format() {
        let info = RouterInfo::from_export_banner(V6_EXPORT);
        assert_eq!(info.version.as_deref(), Some("6.49.10"));
        assert_eq!(info.model.as_deref(), Some("RouterBOARD 750G r3"));
        assert_eq!(info.serial, None);
    }

    #[test]
    fn normalized_output_is_stable_across_captures() {
        // Same config, different clock: the stored bytes must not move.
        let later = V7_EXPORT.replace("2024-01-15 10:22:31", "2024-06-30 23:59:59");
        let first = normalize(V7_EXPORT, "/export terse", "rtr1", &options());
        let second = normalize(&later, "/export terse", "rtr1", &options());
        assert_eq!(first.contents, second.contents);
        assert!(!first.contents.contains("10:22:31"));
    }

    #[test]
    fn firmware_change_shows_up_in_the_file() {
        let upgraded = V7_EXPORT.replace("7.13.2", "7.14.3");
        let before = normalize(V7_EXPORT, "/export terse", "rtr1", &options());
        let after = normalize(&upgraded, "/export terse", "rtr1", &options());
        assert_ne!(before.contents, after.contents);
        assert!(after.contents.contains("# routeros = 7.14.3"));
    }

    #[test]
    fn keeps_configuration_body_intact() {
        let out = normalize(V7_EXPORT, "/export terse", "rtr1", &options());
        assert!(out.contents.contains("/interface bridge"));
        assert!(out.contents.contains("add address=10.0.0.1/24"));
        assert!(out.contents.ends_with("network=10.0.0.0\n"));
    }

    #[test]
    fn only_the_leading_comment_block_is_removed() {
        let raw = "# 2024-01-15 10:22:31 by RouterOS 7.13.2\n\n/system script\nadd name=s source=\"# not a banner\"\n";
        let out = normalize(raw, "/export", "rtr1", &options());
        assert!(out.contents.contains("# not a banner"));
        assert_eq!(out.contents.matches("RouterOS 7.13.2").count(), 0);
        assert!(out.contents.contains("# routeros = 7.13.2"));
    }

    #[test]
    fn crlf_is_normalized() {
        let raw = V7_EXPORT.replace('\n', "\r\n");
        let out = normalize(&raw, "/export terse", "rtr1", &options());
        assert!(!out.contents.contains('\r'));
    }

    #[test]
    fn verbatim_mode_keeps_the_original_banner() {
        let opts = Export {
            normalize_header: false,
            ..options()
        };
        let out = normalize(V7_EXPORT, "/export terse", "rtr1", &opts);
        assert!(
            out.contents
                .contains("# 2024-01-15 10:22:31 by RouterOS 7.13.2")
        );
    }

    const V7_PRINTS_THEN_EXPORT: &str = "\
Flags: A - disabled, H - hidden
 0 A name=\"admin\" group=full password-hash=\"...\" \
   password-hash-nthash=\"...\"

 0 user=\"admin\" key-owner=\"ssh-rsa AAA...\"
# 2024-01-15 10:22:31 by RouterOS 7.13.2
# software id = ABCD-EFGH
#
# model = RB5009UG+S+
# serial number = HGT08XXXXX

/interface bridge
add admin-mac=48:A9:8A:00:00:01 auto-mac=no comment=defconf name=bridge
";

    #[test]
    fn split_separates_user_prints_from_the_export() {
        let (prefix, export_part) = split_at_export_banner(V7_PRINTS_THEN_EXPORT);
        assert!(prefix.contains("name=\"admin\""));
        assert!(prefix.contains("key-owner=\"ssh-rsa"));
        assert!(!prefix.contains("by RouterOS"));
        assert!(export_part.starts_with("# 2024-01-15 10:22:31 by RouterOS 7.13.2"));
        assert!(export_part.contains("/interface bridge"));
    }

    #[test]
    fn output_without_a_banner_goes_whole_into_the_export_part() {
        let raw = "/ip address\nadd address=10.0.0.1/24\n";
        let (prefix, export_part) = split_at_export_banner(raw);
        assert_eq!(prefix, "");
        assert_eq!(export_part, raw);
    }

    #[test]
    fn user_prints_are_kept_as_rem_comments_between_header_and_config() {
        let out = normalize(
            V7_PRINTS_THEN_EXPORT,
            "/user print detail; /user ssh-keys print; /export terse",
            "rtr1",
            &options(),
        );
        // Header first.
        assert!(out.contents.starts_with("# RouterOS configuration export"));
        // Then the user/ssh-keys state as `# REM` lines (right after the bare
        // "#" line that closes the header block) — readable in the file,
        // inert if the .rsc is ever re-imported.
        let header_end = out.contents.find("#\n").expect("end of header block");
        let after_header = &out.contents[header_end + 2..];
        assert!(after_header.starts_with("# REM Flags: A - disabled"));
        assert!(out.contents.contains("# REM  0 A name=\"admin\""));
        assert!(
            out.contents
                .contains("# REM  0 user=\"admin\" key-owner=\"ssh-rsa")
        );
        // No uncommented state lines survive.
        assert!(!out.contents.contains("\nFlags: A - disabled"));
        // Then the stripped config body.
        assert!(out.contents.contains("/interface bridge"));
        assert!(!out.contents.contains("by RouterOS 7.13.2\n"));
    }

    #[test]
    fn parses_system_prints() {
        let resource = "                   uptime: 4w2d3h\n                  version: 7.13.2 (stable)\n               build-time: 2023-11-29 12:12:00\n               board-name: RB5009UG+S+\n        architecture-name: arm64\n";
        let info = RouterInfo::from_resource_print(resource);
        assert_eq!(info.version.as_deref(), Some("7.13.2 (stable)"));
        assert_eq!(info.model.as_deref(), Some("RB5009UG+S+"));
        assert_eq!(info.architecture.as_deref(), Some("arm64"));

        let identity = "  name: core-rtr-01\n";
        assert_eq!(
            RouterInfo::from_identity_print(identity)
                .identity
                .as_deref(),
            Some("core-rtr-01")
        );
    }

    #[test]
    fn merge_prefers_existing_values() {
        let mut info = RouterInfo {
            version: Some("7.13.2".into()),
            ..RouterInfo::default()
        };
        info.merge_from(RouterInfo {
            version: Some("6.49.10".into()),
            serial: Some("ABC".into()),
            ..RouterInfo::default()
        });
        assert_eq!(info.version.as_deref(), Some("7.13.2"));
        assert_eq!(info.serial.as_deref(), Some("ABC"));
    }

    #[test]
    fn empty_export_yields_empty_file_not_a_lone_header() {
        let out = normalize("", "/export", "rtr1", &options());
        assert!(out.contents.contains("# device = rtr1"));
        assert!(out.contents.ends_with('\n'));
    }
}
