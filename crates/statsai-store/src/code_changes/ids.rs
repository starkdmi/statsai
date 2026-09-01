use super::*;

pub(crate) fn new_opaque_committed_metric_id() -> Result<String> {
    let mut random = [0_u8; 32];
    getrandom::getrandom(&mut random).context("generate opaque committed metric id")?;
    Ok(format!("ccm_{}", hex::encode(random)))
}

pub(crate) fn blinded_committed_metric_id(
    identity_key: &[u8; 32],
    repository_hash: &str,
    commit_hash: &str,
) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(identity_key)
        .expect("HMAC accepts fixed-length identity keys");
    mac.update(b"statsai.committed-metric.v1\0");
    update_hmac_field(&mut mac, repository_hash);
    update_hmac_field(&mut mac, commit_hash);
    format!("ccm_{}", hex::encode(mac.finalize().into_bytes()))
}

pub(crate) fn update_hmac_field(mac: &mut Hmac<Sha256>, value: &str) {
    mac.update(&(value.len() as u64).to_be_bytes());
    mac.update(value.as_bytes());
}
