use super::config::WasmConfig;
use super::response::{InstantiateResponse, StoreCodeResponse};
use crate::support::cosmos::ResponseValuePicker;
use crate::support::ops_response::OpResponseDisplay;
use crate::support::state::{State, STATE_DIR, STATE_FILE_LOCAL};
use crate::support::template::Template;
use crate::{framework::Context, support::cosmos::Client};
use anyhow::Context as _;
use anyhow::Result;
use cosmrs::cosmwasm::{MsgInstantiateContract, MsgStoreCode};
use cosmrs::crypto::secp256k1::SigningKey;
use cosmrs::tx::{Fee, Msg};
use std::fs::File;
use std::future::Future;
use std::io::{BufReader, Read};
use std::{env, path::PathBuf, process::Command};

pub fn new<'a, Ctx: Context<'a, WasmConfig>>(
    ctx: &Ctx,
    name: &str,
    version: Option<String>,
    target_dir: Option<PathBuf>,
) -> Result<()> {
    let cfg = ctx.config()?;
    let repo = &cfg.template_repo;
    let version = version.unwrap_or_else(|| "main".to_string());
    let target_dir =
        target_dir.unwrap_or(ctx.root()?.join(PathBuf::from(cfg.contract_dir.as_str())));

    let cw_template = Template::new(name.to_string(), repo.to_owned(), version, target_dir, None);
    cw_template.generate()
}

pub fn build<'a, Ctx: Context<'a, WasmConfig>>(
    ctx: &Ctx,
    optimize: &bool,
    aarch64: &bool,
) -> Result<()> {
    let root = ctx.root()?;

    let wp_name = root.file_name().unwrap().to_str().unwrap(); // handle properly

    env::set_current_dir(&root)?;

    let root_dir_str = root.to_str().unwrap();

    let _build = Command::new("cargo")
        .env(" RUSTFLAGS", "-C link-arg=-s")
        .arg("build")
        .arg("--release")
        .arg("--target=wasm32-unknown-unknown")
        .spawn()?
        .wait()?;

    if *optimize {
        println!("Optimizing wasm...");

        let arch_suffix = if *aarch64 { "-arm64" } else { "" };

        let _optim = Command::new("docker")
            .args(&[
                "run",
                "--rm",
                "-v",
                format!("{root_dir_str}:/code").as_str(),
                "--mount",
                format!("type=volume,source={wp_name}_cache,target=/code/target").as_str(),
                "--mount",
                "type=volume,source=registry_cache,target=/usr/local/cargo/registry",
                format!("cosmwasm/workspace-optimizer{arch_suffix}:0.12.6").as_str(), // TODO: Extract version & check for architecture
            ])
            .spawn()?
            .wait()?;
    }

    Ok(())
}

pub fn store_code<'a, Ctx: Context<'a, WasmConfig>>(
    ctx: &Ctx,
    contract_name: &str,
    chain_id: &str,
    fee: &Fee,
    timeout_height: &u32,
    signing_key: SigningKey,
) -> Result<StoreCodeResponse> {
    let global_config = ctx.global_config()?;
    let account_prefix = global_config.account_prefix().as_str();
    let derivation_path = global_config.derivation_path().as_str();

    let client = Client::local(chain_id, derivation_path)
        .to_signing_client(signing_key, account_prefix.to_string());

    let wasm = read_wasm(ctx, contract_name)?;
    let msg_store_code = MsgStoreCode {
        sender: client.signer_account_id(),
        wasm_byte_code: wasm,
        instantiate_permission: None, // TODO: Add this when working on migration
    }
    .to_any()
    .unwrap();

    block(async {
        let response = client
            .sign_and_broadcast(vec![msg_store_code], fee.clone(), "", timeout_height)
            .await?;

        let code_id: u64 = response.pick("store_code", "code_id").to_string().parse()?;
        let store_code_response = StoreCodeResponse { code_id };

        State::update_state_file(ctx.root()?, &|s: &State| -> State {
            s.update_code_id(chain_id, contract_name, &code_id)
        })?;
        store_code_response.log();

        Ok(store_code_response)
    })
}

#[allow(clippy::too_many_arguments)]
pub fn instantiate<'a, Ctx: Context<'a, WasmConfig>>(
    ctx: &Ctx,
    contract_name: &str,
    label: &str,
    raw: Option<&String>,
    chain_id: &str,
    timeout_height: &u32,
    fee: &Fee,
    signing_key: SigningKey,
) -> Result<InstantiateResponse> {
    let global_config = ctx.global_config()?;
    let account_prefix = global_config.account_prefix().as_str();
    let derivation_path = global_config.derivation_path().as_str();

    let client = Client::local(chain_id, derivation_path)
        .to_signing_client(signing_key, account_prefix.to_string());

    let state = State::load(&ctx.root()?.join(STATE_DIR).join(STATE_FILE_LOCAL))?;
    let code_id = *state.get_ref(chain_id, contract_name)?.code_id();

    let msg_instantiate_contract = MsgInstantiateContract {
        sender: client.signer_account_id(),
        admin: None, // TODO: Fix this when working on migration
        code_id,
        label: Some(label.to_string()),
        msg: raw.map(|s| s.as_bytes().to_vec()).unwrap_or_default(),
        funds: vec![], // TODO: Add options for adding funds
    };

    block(async {
        let response = client
            .sign_and_broadcast(
                vec![msg_instantiate_contract.to_any().unwrap()],
                fee.clone(),
                "",
                timeout_height,
            )
            .await?;

        let contract_address = response
            .pick("instantiate", "_contract_address")
            .to_string();

        let instantiate_response = InstantiateResponse {
            code_id,
            contract_address: contract_address.clone(),
            label: msg_instantiate_contract
                .label
                .unwrap_or_else(|| "-".to_string()),
            creator: msg_instantiate_contract.sender.to_string(),
            admin: msg_instantiate_contract
                .admin
                .map(|a| a.to_string())
                .unwrap_or_else(|| "-".to_string()),
        };

        instantiate_response.log();

        State::update_state_file(ctx.root()?, &|s: &State| -> State {
            s.update_address(chain_id, contract_name, label, &contract_address)
        })?;

        Ok(instantiate_response)
    })
}

fn read_wasm<'a, Ctx: Context<'a, WasmConfig>>(
    ctx: &Ctx,
    contract_name: &str,
) -> Result<Vec<u8>, anyhow::Error> {
    let wasm_path = ctx
        .root()?
        .as_path()
        .join("artifacts")
        .join(format!("{contract_name}.wasm"));
    let wasm_path_str = &wasm_path.as_os_str().to_string_lossy();
    let f = File::open(&wasm_path).with_context(|| {
        format!(
            "`{wasm_path_str}` not found, please build and optimize the contract before store code`"
        )
    })?;
    let mut reader = BufReader::new(f);
    let mut wasm = Vec::new();
    reader.read_to_end(&mut wasm)?;
    Ok(wasm)
}

fn block<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}