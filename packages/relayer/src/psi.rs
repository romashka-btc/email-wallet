use crate::*;

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use num_bigint::RandBigInt;
use serde::{Deserialize, Serialize};
use tokio::fs::{read_to_string, remove_file};
use tokio::sync::mpsc::UnboundedSender;

const DELAY: u64 = 300;

pub(crate) enum UnclaimType {
    Fund(UnclaimedFund),
    State(UnclaimedState),
}

#[derive(Serialize, Deserialize)]
pub(crate) struct CheckRequest {
    pub(crate) point: Point,
    pub(crate) id: U256,
    pub(crate) is_fund: bool,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct RevealRequest {
    pub(crate) id: U256,
    pub(crate) is_fund: bool,
    pub(crate) randomness: String,
    pub(crate) email_address: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct Point {
    pub(crate) x: String,
    pub(crate) y: String,
}

struct PSIClient {
    pub(crate) point: Point,
    pub(crate) random: String,
    pub(crate) email_addr: String,
    pub(crate) recipient_commitment: String,
    pub(crate) chain_client: Arc<ChainClient>,
}

impl PSIClient {
    pub(crate) async fn new(
        chain_client: Arc<ChainClient>,
        email_addr: &str,
        recipient_commitment: String,
    ) -> Result<Self> {
        let mut rng = rand::thread_rng();
        let random = rng.gen_biguint(253);
        let random = Fr::from_bytes(&random.to_bytes_le().try_into().unwrap()).unwrap();
        let random = field2hex(&random);

        let point = psi_step1(CIRCUITS_DIR_PATH.get().unwrap(), email_addr, &random).await?;

        Ok(Self {
            email_addr: email_addr.to_string(),
            recipient_commitment,
            random,
            point,
            chain_client,
        })
    }

    pub(crate) async fn check(&self, client: reqwest::Client, address: &str) -> Result<bool> {
        let res = client
            .post(format!("{}/serveCheck/", address))
            .json(&serde_json::json!({ "point": self.point.clone(), "tx_hash": &self.recipient_commitment }))
            .send()
            .await?
            .error_for_status()?;

        let response_point = res.json::<Point>().await?;

        let result_point = psi_step3(
            CIRCUITS_DIR_PATH.get().unwrap(),
            response_point,
            &self.random,
        )
        .await?;

        self.chain_client
            .check_if_point_registered(result_point)
            .await
    }

    pub(crate) async fn find<'a>(&self) -> Result<Option<String>> {
        let client = reqwest::Client::new();
        let relayers = self.chain_client.get_relayers().await?;

        for relayer in relayers {
            if self.check(client.clone(), &relayer).await? {
                return Ok(Some(relayer));
            }
        }

        Ok(None)
    }

    pub(crate) async fn reveal(
        address: &str,
        randomness: &str,
        recipient_commitment: &str,
    ) -> Result<()> {
        let client = reqwest::Client::new();
        let res = client
            .post(format!("{}/serveReveal/", address))
            .json(&serde_json::json!({ "randomness": randomness, "recipient_commitment": recipient_commitment }))
            .send()
            .await?
            .error_for_status()?;

        Ok(())
    }
}

pub(crate) async fn serve_check_request(
    chain_client: Arc<ChainClient>,
    payload: CheckRequest,
) -> Result<Json<Point>> {
    check_unclaim_valid(Arc::clone(&chain_client), &payload.id, payload.is_fund).await?;

    let res = psi_step2(
        CIRCUITS_DIR_PATH.get().unwrap(),
        payload.point,
        RELAYER_RAND.get().unwrap(),
    )
    .await?;

    Ok(axum::response::Json(res))
}

pub(crate) async fn serve_reveal_request(
    payload: RevealRequest,
    chain_client: Arc<ChainClient>,
    tx_claimer: UnboundedSender<Claim>,
) -> Result<String> {
    match check_unclaim_valid(Arc::clone(&chain_client), &payload.id, payload.is_fund).await? {
        UnclaimType::Fund(unclaimed_fund) => {
            tx_claimer.send(Claim {
                id: payload.id,
                email_address: payload.email_address.clone(),
                random: payload.randomness,
                commit: "0x".to_string() + &hex::encode(unclaimed_fund.email_addr_commit),
                expiry_time: unclaimed_fund.expiry_time.as_u64() as i64,
                is_fund: true,
                is_announced: false,
            })?;
            Ok(format!(
                "Unclaimed fund for {} is accepted",
                payload.email_address
            ))
        }
        UnclaimType::State(unclaimed_state) => {
            tx_claimer.send(Claim {
                id: payload.id,
                email_address: payload.email_address.clone(),
                random: payload.randomness,
                commit: "0x".to_string() + &hex::encode(unclaimed_state.email_addr_commit),
                expiry_time: unclaimed_state.expiry_time.as_u64() as i64,
                is_fund: false,
                is_announced: false,
            })?;
            Ok(format!(
                "Unclaimed state for {} is accepted",
                payload.email_address
            ))
        }
    }
}

pub(crate) async fn check_unclaim_valid(
    chain_client: Arc<ChainClient>,
    id: &U256,
    is_fund: bool,
) -> Result<UnclaimType> {
    // let recipient_commitment = hex2field(commitment)?;
    let current_time = U256::from(now());
    let current_time_delayed = current_time + U256::from(DELAY);
    let unclaim = if is_fund {
        let fund = chain_client.query_unclaimed_fund(*id).await?;
        if fund.expiry_time < current_time_delayed {
            bail!("Unclaimed fund is expired");
        }
        UnclaimType::Fund(fund)
    } else {
        let state = chain_client.query_unclaimed_state(*id).await?;
        if state.expiry_time < current_time_delayed {
            bail!("Unclaimed state is expired");
        }
        UnclaimType::State(state)
    };

    Ok(unclaim)
}

pub(crate) async fn psi_step1(
    circuits_dir_path: &Path,
    email_addr: &str,
    client_rand: &str,
) -> Result<Point> {
    let input_file_name = PathBuf::new()
        .join(INPUT_FILES_DIR.get().unwrap())
        .join(email_addr.to_string() + "psi" + ".json");

    let command_str = format!(
        "--cwd {} psi-step1 --email-addr {} --client-rand {} --output {}",
        circuits_dir_path.to_str().unwrap(),
        email_addr,
        client_rand,
        input_file_name.to_str().unwrap()
    );

    let mut proc = tokio::process::Command::new("yarn")
        .args(command_str.split_whitespace())
        .spawn()?;

    let status = proc.wait().await?;
    assert!(status.success());

    let result = read_to_string(&input_file_name).await?;
    remove_file(input_file_name).await?;

    let point: Point = serde_json::from_str(&result)?;

    Ok(point)
}

pub(crate) async fn psi_step2(
    circuits_dir_path: &Path,
    point: Point,
    relayer_rand: &str,
) -> Result<Point> {
    let input_file_name = calculate_default_hash(&point.x);
    let input_file_name = PathBuf::new()
        .join(INPUT_FILES_DIR.get().unwrap())
        .join(input_file_name + ".json");

    let command_str = format!(
        "--cwd {} psi-step2 --x {} --y {} --relayer-rand {} --output {}",
        circuits_dir_path.to_str().unwrap(),
        point.x,
        point.y,
        relayer_rand,
        input_file_name.to_str().unwrap()
    );

    let mut proc = tokio::process::Command::new("yarn")
        .args(command_str.split_whitespace())
        .spawn()?;

    let status = proc.wait().await?;
    assert!(status.success());

    let result = read_to_string(&input_file_name).await?;
    remove_file(input_file_name).await?;

    let point: Point = serde_json::from_str(&result)?;

    Ok(point)
}

pub(crate) async fn psi_step3(
    circuits_dir_path: &Path,
    point: Point,
    client_rand: &str,
) -> Result<Point> {
    let input_file_name = calculate_default_hash(&point.x);
    let input_file_name = PathBuf::new()
        .join(INPUT_FILES_DIR.get().unwrap())
        .join(input_file_name + ".json");

    let command_str = format!(
        "--cwd {} psi-step3 --x {} --y {} --client-rand {} --output {}",
        circuits_dir_path.to_str().unwrap(),
        point.x,
        point.y,
        client_rand,
        input_file_name.to_str().unwrap()
    );

    let mut proc = tokio::process::Command::new("yarn")
        .args(command_str.split_whitespace())
        .spawn()?;

    let status = proc.wait().await?;
    assert!(status.success());

    let result = read_to_string(&input_file_name).await?;
    remove_file(input_file_name).await?;

    let point: Point = serde_json::from_str(&result)?;

    Ok(point)
}
