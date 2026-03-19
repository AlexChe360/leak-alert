use anyhow::{anyhow, Context, Result};
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_serial::{SerialPortBuilderExt, SerialStream};
use tracing::{error, info, warn};
use tokio_serial::{FlowControl, DataBits, Parity, StopBits};

#[derive(Debug, Deserialize)]
struct LeakPayload {
    water_leak: Option<bool>,
    battery: Option<u8>,
    voltage: Option<u32>,
    tamper: Option<bool>,
    linkquality: Option<u32>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .init();

    let mqtt_host = env("MQTT_HOST", "127.0.0.1");
    let mqtt_port: u16 = env("MQTT_PORT", "1883")
        .parse()
        .context("MQTT_PORT parse error")?;
    let mqtt_topic = env("MQTT_TOPIC", "zigbee2mqtt/+");
    let serial_port = env("SERIAL_PORT", "/dev/ttyUSB4");
    let phone_number = env("PHONE_NUMBER", "+77078185115");
    let sms_cooldown_secs: u64 = env("SMS_COOLDOWN_SECS", "300")
        .parse()
        .context("SMS_COOLDOWN_SECS parse error")?;

    let mut mqttoptions = MqttOptions::new("leak-alert-service", mqtt_host.clone(), mqtt_port);
    mqttoptions.set_keep_alive(Duration::from_secs(30));

    let (client, mut eventloop) = AsyncClient::new(mqttoptions, 10);
    client.subscribe(mqtt_topic.clone(), QoS::AtLeastOnce).await?;

    info!("Connected to MQTT {}:{}, topic={}", mqtt_host, mqtt_port, mqtt_topic);
    info!("Using modem port {}", serial_port);

    let mut last_sent: HashMap<String, Instant> = HashMap::new();
    let cooldown = Duration::from_secs(sms_cooldown_secs);

    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Incoming::Publish(p))) => {
                let topic = p.topic.clone();

                if topic.starts_with("zigbee2mqtt/bridge/") {
                    continue;
                }

                let payload = match String::from_utf8(p.payload.to_vec()) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Invalid UTF-8 on topic {}: {}", topic, e);
                        continue;
                    }
                };

                let data: LeakPayload = match serde_json::from_str(&payload) {
                    Ok(v) => v,
                    Err(_) => {
                        continue;
                    }
                };

                if data.water_leak != Some(true) {
                    continue;
                }

                let now = Instant::now();
                if let Some(last) = last_sent.get(&topic) {
                    if now.duration_since(*last) < cooldown {
                        info!("Skip duplicate alert for {}", topic);
                        continue;
                    }
                }

                let message = format!(
                    "ALERT: Water leak detected!\nDevice: {}\nBattery: {:?}\nVoltage: {:?}\nTamper: {:?}\nLQI: {:?}",
                    topic,
                    data.battery,
                    data.voltage,
                    data.tamper,
                    data.linkquality
                );

                info!("Leak detected on {}", topic);

                match send_sms(&serial_port, &phone_number, &message).await {
                    Ok(_) => {
                        info!("SMS sent to {}", phone_number);
                        last_sent.insert(topic, now);
                    }
                    Err(e) => {
                        error!("SMS send failed: {:?}", e);
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                error!("MQTT error: {:?}", e);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

async fn send_sms(port_path: &str, phone: &str, text: &str) -> Result<()> {
    let builder = tokio_serial::new(port_path, 115200)
        .data_bits(DataBits::Eight)
        .parity(Parity::None)
        .stop_bits(StopBits::One)
        .flow_control(FlowControl::None);

    let mut port = builder
        .open_native_async()
        .with_context(|| format!("Failed to open serial port {}", port_path))?;

    drain_port(&mut port).await?;

    let r = at(&mut port, "AT\r\n", "OK").await?;
    info!("AT => {:?}", r);

    let r = at(&mut port, "AT+CMEE=2\r\n", "OK").await?;
    info!("AT+CMEE=2 => {:?}", r);

    let r = at(&mut port, "AT+CPIN?\r\n", "OK").await?;
    info!("AT+CPIN? => {:?}", r);

    let r = at(&mut port, "AT+CSQ\r\n", "OK").await?;
    info!("AT+CSQ => {:?}", r);

    let r = at(&mut port, "AT+CREG?\r\n", "OK").await?;
    info!("AT+CREG? => {:?}", r);

    let r = at(&mut port, "AT+CMGF=1\r\n", "OK").await?;
    info!("AT+CMGF=1 => {:?}", r);

    let r = at(&mut port, "AT+CSCA?\r\n", "OK").await?;
    info!("AT+CSCA? => {:?}", r);

    tokio::time::sleep(Duration::from_secs(2)).await;
    drain_port(&mut port).await?;

    let cmd = format!("AT+CMGS=\"{}\"\r\n", phone);
    let prompt_resp = write_and_wait_for_prompt(&mut port, &cmd).await?;
    info!("AT+CMGS prompt => {:?}", prompt_resp);

    tokio::time::sleep(Duration::from_millis(500)).await;
    port.write_all(text.as_bytes()).await?;
    port.write_all(&[26]).await?;
    port.flush().await?;

    let resp = read_response(&mut port, Duration::from_secs(20)).await?;
    info!("AT+CMGS final => {:?}", resp);

    if resp.contains("+CMGS") && resp.contains("OK") {
        Ok(())
    } else {
        Err(anyhow!("SMS send failed: {}", resp))
    }
}

async fn drain_port(port: &mut SerialStream) -> Result<()> {
    let mut buf = [0u8; 512];

    loop {
        match tokio::time::timeout(Duration::from_millis(150), port.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => continue,
            Ok(Ok(_)) => break,
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }

    Ok(())
}

async fn at(port: &mut SerialStream, cmd: &str, expected: &str) -> Result<String> {
    drain_port(port).await?;

    port.write_all(cmd.as_bytes()).await?;
    port.flush().await?;

    let resp = read_response(port, Duration::from_secs(5)).await?;
    if resp.contains(expected) {
        Ok(resp)
    } else {
        Err(anyhow!("AT failed: cmd={} resp={:?}", cmd.trim(), resp))
    }
}

async fn write_and_wait_for_prompt(port: &mut SerialStream, cmd: &str) -> Result<String> {
    drain_port(port).await?;

    port.write_all(cmd.as_bytes()).await?;
    port.flush().await?;

    let start = Instant::now();
    let mut buf = [0u8; 256];
    let mut out = Vec::new();

    while start.elapsed() < Duration::from_secs(15) {
        match tokio::time::timeout(Duration::from_millis(500), port.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => {
                out.extend_from_slice(&buf[..n]);
                let s = String::from_utf8_lossy(&out).to_string();

                if s.contains('>') {
                    return Ok(s);
                }

                if s.contains("ERROR") || s.contains("+CMS ERROR") {
                    return Err(anyhow!("CMGS failed before prompt: {:?}", s));
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(anyhow!("Serial read error: {}", e)),
            Err(_) => {}
        }
    }

    Err(anyhow!(
        "No SMS prompt received, raw modem response: {:?}",
        String::from_utf8_lossy(&out).to_string()
    ))
}

async fn read_response(port: &mut SerialStream, timeout: Duration) -> Result<String> {
    let start = Instant::now();
    let mut buf = [0u8; 1024];
    let mut out = Vec::new();

    while start.elapsed() < timeout {
        match tokio::time::timeout(Duration::from_millis(500), port.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => {
                out.extend_from_slice(&buf[..n]);
                let s = String::from_utf8_lossy(&out);

                if s.contains("OK")
                    || s.contains("ERROR")
                    || s.contains("+CMS ERROR")
                    || s.contains("+CMGS")
                    || s.contains('>')
                {
                    return Ok(s.to_string());
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(anyhow!("Serial read error: {}", e)),
            Err(_) => {}
        }
    }

    Ok(String::from_utf8_lossy(&out).to_string())
}