use color_eyre::{Result, eyre::WrapErr};
use lettre::message::{Mailbox, SinglePart};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use std::time::Duration;
use tokio::time::sleep;

const RETRY_COUNT: u32 = 3;

pub struct Mailer {
    pub from: Mailbox,
    pub transport: AsyncSmtpTransport<Tokio1Executor>,
}

pub struct Mail {
    pub subject: String,
    pub body: String,
}

pub async fn send_email_with_backoff(
    sender: &Mailer,
    to: &[Mailbox],
    cc: &[Mailbox],
    bcc: &[Mailbox],
    mails: Vec<Mail>,
) -> Result<()> {
    let mut message = Message::builder().from(sender.from.clone());

    for addr in to.iter() {
        message = message.to(addr.clone());
    }

    for addr in cc.iter() {
        message = message.cc(addr.clone());
    }

    for addr in bcc.iter() {
        message = message.bcc(addr.clone());
    }

    for mail in mails {
        let message = message
            .clone()
            .subject(mail.subject)
            .singlepart(SinglePart::html(mail.body))
            .wrap_err("Failed to build message")?;

        for attempt in 1..=RETRY_COUNT {
            match sender.transport.send(message.clone()).await {
                Ok(_) => break,
                Err(e) if attempt < RETRY_COUNT => {
                    log::warn!("Failed to send email (attempt {attempt}): {e}");
                    sleep(Duration::from_secs(1 << attempt)).await;
                }
                Err(e) => {
                    return Err(e).wrap_err("Failed to send email after all retries");
                }
            }
        }
    }

    Ok(())
}
