//! Email notifications: the report DonDude mails to the NOC after a
//! scheduled run.
//!
//! Multipart alternative (plain text + HTML): the text part keeps every
//! consumer that greps mailboxes happy, the HTML part gives the NOC a
//! scannable colored table. The subject line stays exactly
//! `DonDude backup: N failed, M changed` — external automation parses it.
//! The settings hold the SMTP relay coordinates and credentials (sealed
//! with the master key, like every other secret); the trigger is the
//! scheduler — manual and CLI runs do not send mail, so clicking
//! "Back up all devices now" twice never spams anyone.

use crate::backup::RunReport;
use lettre::message::{Mailbox, MultiPart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::Tls;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::error::Result;

/// Everything `send_report` needs, straight from the settings row.
#[derive(Debug, Clone)]
pub struct MailConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    /// The unsealed SMTP password.
    pub password: String,
    pub from: String,
    pub to: String,
    /// true: mail only when the run failed something.
    pub failure_only: bool,
}

/// The HTML entity for one special character, assembled char by char so no
/// literal "&...;" sequence appears anywhere in this source file (the
/// deployment toolchain decodes such sequences and would corrupt them).
fn entity(c: char) -> String {
    let name = match c {
        '&' => "amp",
        '<' => "lt",
        '>' => "gt",
        '"' => "quot",
        _ => return c.to_string(),
    };
    let mut e = String::with_capacity(name.len() + 2);
    e.push('&');
    e.push_str(name);
    e.push(';');
    e
}

/// Minimal HTML escaping for values interpolated into the report page.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' | '<' | '>' | '"' => out.push_str(&entity(c)),
            _ => out.push(c),
        }
    }
    out
}

/// Render the run as a plain-text body. Per-device lines first — the part a
/// NOC actually scans — then the tally and the push status.
pub(crate) fn render_body(report: &RunReport) -> String {
    let mut out = String::new();
    out.push_str("DonDude scheduled backup report\n\n");

    for device in &report.devices {
        out.push_str(&format!(
            "{:<24} {:<10} {}\n",
            device.device,
            device.outcome.label(),
            device.detail()
        ));
    }

    out.push('\n');
    out.push_str(&report.summary());
    out.push('\n');
    match &report.push {
        crate::backup::PushReport::Pushed => out.push_str("Push: ok\n"),
        crate::backup::PushReport::Skipped(reason) => {
            out.push_str(&format!("Push: skipped ({reason})\n"))
        }
        crate::backup::PushReport::Failed(error) => {
            out.push_str(&format!("Push: FAILED ({error})\n"))
        }
    }
    out
}

/// Same content as `render_body`, as a self-contained styled HTML page.
/// Inline CSS only: email clients strip <style> blocks. The look matches the
/// Proxmox morning report (blue #2980b9 headers, green #27ae60 / red #e74c3c
/// statuses, light-grey data blocks).
pub(crate) fn render_html(report: &RunReport) -> String {
    let failed = report.failed();
    let banner_bg = if failed > 0 { "#fdedec" } else { "#e8f8f5" };
    let banner_border = if failed > 0 { "#e74c3c" } else { "#27ae60" };
    let banner_text = if failed > 0 { "#c0392b" } else { "#1e8449" };
    let verdict = if failed > 0 {
        format!("ATTENTION: {failed} device(s) failed")
    } else {
        "SUCCESS: all backups completed".to_string()
    };

    let started_local = report
        .started_at
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S %Z");

    // One table row per device: name, colored outcome badge, detail.
    let mut rows = String::new();
    for d in &report.devices {
        let (label, color, bg) = match &d.outcome {
            crate::backup::Outcome::Failed(_) => ("failed", "#e74c3c", "#fdedec"),
            crate::backup::Outcome::Committed(_) => ("committed", "#27ae60", "#e8f8f5"),
            crate::backup::Outcome::WouldChange => ("would change", "#e67e22", "#fdf2e9"),
            crate::backup::Outcome::Unchanged => ("unchanged", "#7f8c8d", "#f8f9fa"),
        };
        rows.push_str(&format!(
            "<tr>\
<td style=\"padding:8px 12px;border-bottom:1px solid #eee;font-weight:600;\">{}</td>\
<td style=\"padding:8px 12px;border-bottom:1px solid #eee;white-space:nowrap;\">\
<span style=\"background:{};color:{};padding:3px 8px;border-radius:4px;font-size:13px;font-weight:bold;\">{}</span></td>\
<td style=\"padding:8px 12px;border-bottom:1px solid #eee;color:#555;font-size:13px;\">{}</td>\
</tr>",
            html_escape(&d.device),
            bg,
            color,
            label,
            html_escape(&d.detail())
        ));
    }

    let push_html = match &report.push {
        crate::backup::PushReport::Pushed => {
            "<span style=\"color:#27ae60;font-weight:bold;\">Push: ok</span>".to_string()
        }
        crate::backup::PushReport::Skipped(reason) => {
            format!(
                "<span style=\"color:#7f8c8d;\">Push: skipped ({})</span>",
                html_escape(reason)
            )
        }
        crate::backup::PushReport::Failed(error) => {
            format!(
                "<span style=\"color:#e74c3c;font-weight:bold;\">Push: FAILED ({})</span>",
                html_escape(error)
            )
        }
    };

    let dry_run_badge = if report.dry_run {
        " <span style=\"background:#e67e22;color:#fff;padding:2px 8px;border-radius:4px;font-size:12px;\">DRY RUN</span>"
    } else {
        ""
    };

    format!(
        "<html>\
<body style=\"font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;line-height:1.6;color:#333;max-width:700px;margin:0 auto;padding:20px;\">\
<div style=\"border-bottom:2px solid #3498db;padding-bottom:10px;margin-bottom:20px;\">\
<h2 style=\"color:#2c3e50;margin:0;\">DonDude Backup Report{}</h2>\
<p style=\"color:#7f8c8d;margin:5px 0 0 0;font-size:14px;\">MikroTik RouterOS fleet &mdash; scheduled run of {}</p>\
</div>\
<div style=\"background-color:{};border-left:4px solid {};padding:15px;border-radius:0 4px 4px 0;margin-bottom:20px;\">\
<p style=\"margin:0;color:{};font-size:16px;font-weight:bold;\">{}</p>\
<p style=\"margin:5px 0 0 0;color:#555;font-size:14px;\">{}</p>\
</div>\
<h3 style=\"color:#2980b9;margin-top:30px;border-bottom:1px solid #eee;padding-bottom:5px;\">Devices</h3>\
<table style=\"border-collapse:collapse;width:100%;max-width:700px;\">\
<tr style=\"background:#f8f9fa;\">\
<th style=\"text-align:left;padding:8px 12px;color:#2c3e50;font-size:14px;border-bottom:2px solid #e9ecef;\">Device</th>\
<th style=\"text-align:left;padding:8px 12px;color:#2c3e50;font-size:14px;border-bottom:2px solid #e9ecef;\">Outcome</th>\
<th style=\"text-align:left;padding:8px 12px;color:#2c3e50;font-size:14px;border-bottom:2px solid #e9ecef;\">Detail</th>\
</tr>{}</table>\
<p style=\"margin-top:20px;font-size:14px;\">{}</p>\
<div style=\"margin-top:40px;padding-top:15px;border-top:1px solid #eee;text-align:center;\">\
<p style=\"font-size:12px;color:#95a5a6;margin:0;\">Generated automatically by DonDude &mdash; RouterOS fleet backups</p>\
</div>\
</body>\
</html>",
        dry_run_badge,
        started_local,
        banner_bg,
        banner_border,
        banner_text,
        verdict,
        html_escape(&report.summary()),
        rows,
        push_html,
    )
}

/// Send the report as multipart (plain + HTML). One attempt, no retry loop:
/// mail is best-effort, and a stuck notification must never hold up the next
/// backup run.
pub async fn send_report(config: &MailConfig, report: &RunReport) -> Result<()> {
    let from: Mailbox = config
        .from
        .parse()
        .map_err(|e| crate::error::Error::config(format!("bad from address: {e}")))?;
    let to: Mailbox = config
        .to
        .parse()
        .map_err(|e| crate::error::Error::config(format!("bad to address: {e}")))?;

    let email = Message::builder()
        .from(from)
        .to(to)
        .subject(format!(
            "DonDude backup: {} failed, {} changed",
            report.failed(),
            report.changed()
        ))
        .multipart(MultiPart::alternative_plain_html(
            render_body(report),
            render_html(report),
        ))?;

    // Port 465 speaks implicit TLS ("SMTPS"): the TLS handshake happens
    // before any SMTP line. relay() alone would try STARTTLS instead.
    let tls = lettre::transport::smtp::client::TlsParameters::new(config.host.clone())?;
    let transport = AsyncSmtpTransport::<Tokio1Executor>::relay(&config.host)?
        .port(config.port)
        .credentials(Credentials::new(
            config.username.clone(),
            config.password.clone(),
        ))
        .tls(Tls::Wrapper(tls))
        .build();

    transport.send(email).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::Outcome;

    #[test]
    fn body_carries_the_tally_and_push_status() {
        let report = RunReport {
            started_at: chrono::Utc::now(),
            elapsed: std::time::Duration::from_secs(3),
            devices: vec![],
            sync: None,
            push: crate::backup::PushReport::Skipped("no remote"),
            dry_run: false,
        };
        let body = render_body(&report);
        assert!(body.contains("0 changed, 0 unchanged, 0 failed"));
        assert!(body.contains("Push: skipped (no remote)"));
    }

    #[test]
    fn body_lists_each_device() {
        let report = RunReport {
            started_at: chrono::Utc::now(),
            elapsed: std::time::Duration::from_secs(3),
            devices: vec![crate::backup::DeviceReport {
                device: "core-rtr-01".into(),
                device_id: uuid::Uuid::nil(),
                tenant_id: uuid::Uuid::nil(),
                host: "10.0.0.1".into(),
                tenant: "acme".into(),
                path: "acme/core-rtr-01.rsc".into(),
                firmware: None,
                model: None,
                identity: None,
                serial: None,
                outcome: Outcome::Failed("cannot reach 10.0.0.1".into()),
                elapsed: std::time::Duration::from_secs(10),
            }],
            sync: None,
            push: crate::backup::PushReport::Pushed,
            dry_run: false,
        };
        let body = render_body(&report);
        assert!(body.contains("core-rtr-01"));
        assert!(body.contains("failed"));
        assert!(body.contains("Push: ok"));
    }

    #[test]
    fn html_shows_verdict_and_devices() {
        let report = RunReport {
            started_at: chrono::Utc::now(),
            elapsed: std::time::Duration::from_secs(12),
            devices: vec![
                crate::backup::DeviceReport {
                    device: "edge-sw-01".into(),
                    device_id: uuid::Uuid::nil(),
                    tenant_id: uuid::Uuid::nil(),
                    host: "10.0.0.2".into(),
                    tenant: "acme".into(),
                    path: "acme/edge-sw-01.rsc".into(),
                    firmware: None,
                    model: None,
                    identity: None,
                    serial: None,
                    outcome: Outcome::Unchanged,
                    elapsed: std::time::Duration::from_secs(2),
                },
                crate::backup::DeviceReport {
                    device: "core-rtr-01".into(),
                    device_id: uuid::Uuid::nil(),
                    tenant_id: uuid::Uuid::nil(),
                    host: "10.0.0.1".into(),
                    tenant: "acme".into(),
                    path: "acme/core-rtr-01.rsc".into(),
                    firmware: None,
                    model: None,
                    identity: None,
                    serial: None,
                    outcome: Outcome::Failed("cannot reach 10.0.0.1".into()),
                    elapsed: std::time::Duration::from_secs(10),
                },
            ],
            sync: None,
            push: crate::backup::PushReport::Pushed,
            dry_run: false,
        };
        let html = render_html(&report);
        // Failure banner turns red and mentions the count.
        assert!(html.contains("#fdedec"));
        assert!(html.contains("ATTENTION: 1 device(s) failed"));
        // Device rows carry names and colored badges.
        assert!(html.contains("edge-sw-01"));
        assert!(html.contains("core-rtr-01"));
        assert!(html.contains(">unchanged<"));
        assert!(html.contains(">failed<"));
        // Push status and the summary tally are present.
        assert!(html.contains("Push: ok"));
        assert!(html.contains("2 device(s)"));
    }

    #[test]
    fn html_is_escaped_and_green_on_success() {
        let report = RunReport {
            started_at: chrono::Utc::now(),
            elapsed: std::time::Duration::from_secs(1),
            devices: vec![crate::backup::DeviceReport {
                device: "rtr<script>".into(),
                device_id: uuid::Uuid::nil(),
                tenant_id: uuid::Uuid::nil(),
                host: "10.0.0.3".into(),
                tenant: "acme".into(),
                path: "acme/rtr.rsc".into(),
                firmware: None,
                model: None,
                identity: None,
                serial: None,
                outcome: Outcome::Unchanged,
                elapsed: std::time::Duration::from_secs(1),
            }],
            sync: None,
            push: crate::backup::PushReport::Pushed,
            dry_run: false,
        };
        let html = render_html(&report);
        assert!(html.contains("SUCCESS: all backups completed"));
        // The raw tag never appears; the escaped form does.
        let raw = format!("{}script{}", entity('<'), entity('>'));
        assert!(!html.contains("rtr<script>"));
        assert!(html.contains(&format!("rtr{}", raw)));
    }
}
