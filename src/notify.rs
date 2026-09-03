//! Email notifications: the report DonDude mails to the NOC after a
//! scheduled run.
//!
//! Deliberately text-only. The settings hold the SMTP relay coordinates and
//! credentials (sealed with the master key, like every other secret); the
//! trigger is the scheduler — manual and CLI runs do not send mail, so
//! clicking "Back up all devices now" twice never spams anyone.

use crate::backup::RunReport;
use lettre::message::header::ContentType;
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

/// Send the report. One attempt, no retry loop: mail is best-effort, and a
/// stuck notification must never hold up the next backup run.
pub async fn send_report(config: &MailConfig, report: &RunReport) -> Result<()> {
    let email = Message::builder()
        .from(
            config
                .from
                .parse()
                .map_err(|e| crate::error::Error::config(format!("bad from address: {e}")))?,
        )
        .to(config
            .to
            .parse()
            .map_err(|e| crate::error::Error::config(format!("bad to address: {e}")))?)
        .subject(format!(
            "DonDude backup: {} failed, {} changed",
            report.failed(),
            report.changed()
        ))
        .header(ContentType::TEXT_PLAIN)
        .body(render_body(report))?;

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
}
