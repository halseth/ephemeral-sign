use std::collections::BTreeMap;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;

use bitcoin::address::script_pubkey::ScriptBufExt;
use bitcoin::witness::WitnessExt;
use clap::Parser;

use bitcoin::bip32::KeySource;
use bitcoin::consensus_validation::TransactionExt;
use bitcoin::locktime::absolute;
use bitcoin::psbt::Input;
use bitcoin::secp256k1::{Keypair, Secp256k1, SecretKey, Signing};
use bitcoin::{
    Address, Amount, Network, OutPoint, PrivateKey, Psbt, ScriptBuf, Sequence,
    TapSighashType, Transaction, TxIn, TxOut, Witness, consensus,
    transaction,
};
use shared::{SignPsbtReq, SignPsbtResp};

fn parse_address(addr: &str, network: Network) -> Address {
    Address::from_str(addr)
        .expect("a valid address")
        .require_network(network)
        .expect("valid address for network")
}

fn gen_keypair<C: Signing>(secp: &Secp256k1<C>) -> Keypair {
    let sk = SecretKey::new(&mut rand::thread_rng());
    Keypair::from_secret_key(secp, &sk)
}

#[derive(Debug, Parser)]
#[command(verbatim_doc_comment)]
struct Args {
    #[arg(long)]
    prevout: OutPoint,

    #[arg(long)]
    prev_amt: Amount,

    #[arg(long)]
    fallback_addr: String,

    #[arg(long)]
    output_amt: Amount,

    #[arg(long)]
    change_addr: Option<String>,

    #[arg(long)]
    change_amt: Option<Amount>,

    #[arg(long)]
    client_url: Option<SocketAddr>,

    /// Sign the message using the given private key. Pass "new" to generate one at random. Leave
    /// this blank if verifying a receipt.
    #[arg(long)]
    priv_key: Option<String>,

    /// Network to use.
    #[arg(long, default_value_t = Network::Signet)]
    network: Network,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let secp = Secp256k1::new();
    let network = args.network;

    // Generate a new keypair or use the given private key.
    let (keypair, script_pub) = match args.priv_key.as_deref() {
        Some(priv_str) => {
            let keypair = if priv_str == "new" {
                gen_keypair(&secp)
            } else {
                let sk = SecretKey::from_str(&priv_str).unwrap();
                Keypair::from_secret_key(&secp, &sk)
            };

            let (internal_key, _parity) = keypair.x_only_public_key();
            let script_buf = ScriptBuf::new_p2tr(&secp, internal_key, None);
            let addr = Address::from_script(script_buf.as_script(), network).unwrap();
            println!("priv: {}", hex::encode(keypair.secret_key().secret_bytes()));
            println!("pub: {}", internal_key);
            println!("address: {}", addr);

            if priv_str == "new" {
                return;
            }

            (keypair, addr.script_pubkey())
        }
        _ => {
            println!("priv key needed");
            return;
        }
    };

    //    // Address the presigned tx will send coins to.
    let fallback_addr = parse_address(&args.fallback_addr, args.network);

    let deposit_prevout = TxOut {
        value: args.prev_amt,
        script_pubkey: script_pub,
    };

    let utxos: Vec<TxOut> = vec![deposit_prevout.clone()];
    println!(
        "prevout: {}",
        hex::encode(consensus::encode::serialize(&utxos[0]))
    );

    // Input to deposit.
    let input = TxIn {
        previous_output: args.prevout,
        script_sig: ScriptBuf::default(),
        sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
        witness: Witness::default(),
    };

    // The output the deposit will go into. Note that the output script is not yet determined at
    // this point.
    let deposit_output = TxOut {
        value: args.output_amt,
        script_pubkey: ScriptBuf::default(),
    };

    // The change output is locked to a key controlled by us.
    let change = match args.change_addr {
        None => None,
        Some(addr) => {
            let a = parse_address(&addr, args.network);
            Some(TxOut {
                value: args.change_amt.unwrap(),
                script_pubkey: a.script_pubkey(),
            })
        }
    };

    let outputs = match change {
        None => vec![deposit_output],
        Some(c) => vec![deposit_output, c],
    };

    // The transaction we want to sign and broadcast.
    let unsigned_tx = Transaction {
        version: transaction::Version::TWO,  // Post BIP 68.
        lock_time: absolute::LockTime::ZERO, // Ignore the locktime.
        input: vec![input],                  // Input is 0-indexed.
        output: outputs,                     // Outputs, order does not matter.
    };

    // Now we'll start the PSBT workflow.
    // Step 1: Creator role; that creates,
    // and add inputs and outputs to the PSBT.
    let psbt = Psbt::from_unsigned_tx(unsigned_tx).expect("Could not create PSBT");

    let resp = initiate_sign(args.client_url.unwrap(), &psbt, fallback_addr.to_string())
        .await
        .unwrap();
    let presigned_tx = resp.spend_psbt.extract_tx().expect("valid tx");
    let mut deposit_psbt = resp.deposit_psbt;
    let serialized_presigned_tx = consensus::encode::serialize_hex(&presigned_tx);
    println!("Presigned Details: {:#?}", presigned_tx);
    println!("Raw presigned Transaction: {}", serialized_presigned_tx);

    let mut key_map: HashMap<bitcoin::XOnlyPublicKey, PrivateKey> = HashMap::new();
    let (xpub, _) = keypair.x_only_public_key();
    let sk = PrivateKey::new(keypair.secret_key(), args.network);
    key_map.insert(xpub, sk);

    let mut deposit_origin_input = BTreeMap::new();
    deposit_origin_input.insert(xpub, (vec![], KeySource::default()));

    // Now that we have the presigned spend, we can sign the deposit.
    let ty = TapSighashType::All.into();
    deposit_psbt.inputs = vec![Input {
        witness_utxo: Some(deposit_prevout.clone()),
        tap_key_origins: deposit_origin_input,
        tap_internal_key: Some(xpub),
        sighash_type: Some(ty),
        ..Default::default()
    }];

    deposit_psbt.sign(&key_map, &secp).expect("able to sign");
    deposit_psbt.inputs.iter_mut().for_each(|input| {
        let script_witness = Witness::p2tr_key_spend(&input.tap_key_sig.unwrap());
        input.final_script_witness = Some(script_witness);

        // Clear all the data fields as per the spec.
        input.partial_sigs = BTreeMap::new();
        input.sighash_type = None;
        input.redeem_script = None;
        input.witness_script = None;
        input.bip32_derivation = BTreeMap::new();
    });

    println!("Deposit PSBT: {:#?}", deposit_psbt);

    let signed_tx = deposit_psbt.extract_tx().expect("valid transaction");

    let serialized_signed_tx = consensus::encode::serialize_hex(&signed_tx);
    println!("Deposit Details: {:#?}", signed_tx);
    // check with:
    // bitcoin-cli decoderawtransaction <RAW_TX> true
    println!("Raw deposit Transaction: {}", serialized_signed_tx);

    let res = signed_tx
        .verify(|op| {
            println!("fetchin op {}", op);
            Some(utxos[0].clone())
        })
        .unwrap();
    println!("Transaction Result: {:#?}", res);

    // TODO: verify presigned tx before signing
    let res = presigned_tx
        .verify(|op| {
            println!("fetchin op {}", op);
            Some(signed_tx.output[0].clone())
        })
        .unwrap();
    println!("Pre-signed Transaction Result: {:#?}", res);
}

async fn initiate_sign(
    client_addr: SocketAddr,
    psbt: &Psbt,
    fallback_addr: String,
) -> Result<SignPsbtResp, reqwest::Error> {
    let client = reqwest::Client::new();
    let url = format!("http://{}/psbt", client_addr);
    println!("url: {}", url);

    let body_json = serde_json::to_string(&psbt).unwrap();
    println!("body_json: {}", body_json);
    let body = SignPsbtReq {
        psbt: psbt.clone(),
        fallback_addr: fallback_addr,
    };
    let resp = client
        .post(url)
        .json(&body)
        .send()
        .await?
        .json::<SignPsbtResp>()
        .await?;
    println!("{resp:#?}");

    Ok(resp)
}
