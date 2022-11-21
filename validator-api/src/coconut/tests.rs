// Copyright 2022 - Nym Technologies SA <contact@nymtech.net>
// SPDX-License-Identifier: Apache-2.0

use super::InternalSignRequest;
use crate::coconut::error::{CoconutError, Result};
use coconut_bandwidth_contract_common::events::{
    DEPOSITED_FUNDS_EVENT_TYPE, DEPOSIT_ENCRYPTION_KEY, DEPOSIT_IDENTITY_KEY, DEPOSIT_INFO,
    DEPOSIT_VALUE,
};
use coconut_bandwidth_contract_common::spend_credential::{
    SpendCredential, SpendCredentialResponse,
};
use coconut_interface::{hash_to_scalar, Credential, VerificationKey};
use config::defaults::VOUCHER_INFO;
use cosmwasm_std::{to_binary, Addr, CosmosMsg, Decimal, WasmMsg};
use credentials::coconut::bandwidth::BandwidthVoucher;
use credentials::coconut::params::{
    ValidatorApiCredentialEncryptionAlgorithm, ValidatorApiCredentialHkdfAlgorithm,
};
use crypto::shared_key::recompute_shared_key;
use crypto::symmetric::stream_cipher;
use multisig_contract_common::msg::ProposalResponse;
use nymcoconut::tests::helpers::theta_from_keys_and_attributes;
use nymcoconut::{
    prepare_blind_sign, ttp_keygen, Base58, BlindSignRequest, BlindedSignature, KeyPair, Parameters,
};
use validator_api_requests::coconut::{
    BlindSignRequestBody, BlindedSignatureResponse, CosmosAddressResponse, VerificationKeyResponse,
    VerifyCredentialBody, VerifyCredentialResponse,
};
use validator_client::nymd::Coin;
use validator_client::nymd::{tx::Hash, AccountId, DeliverTx, Event, Fee, Tag, TxResponse};
use validator_client::validator_api::routes::{
    API_VERSION, BANDWIDTH, COCONUT_BLIND_SIGN, COCONUT_COSMOS_ADDRESS,
    COCONUT_PARTIAL_BANDWIDTH_CREDENTIAL, COCONUT_ROUTES, COCONUT_VERIFICATION_KEY,
    COCONUT_VERIFY_BANDWIDTH_CREDENTIAL,
};

use crate::coconut::State;
use crate::ValidatorApiStorage;
use async_trait::async_trait;
use coconut_dkg_common::dealer::{ContractDealing, DealerDetails, DealerDetailsResponse};
use coconut_dkg_common::types::{EncodedBTEPublicKeyWithProof, EpochState};
use contracts_common::dealings::ContractSafeBytes;
use crypto::asymmetric::{encryption, identity};
use rand_07::rngs::OsRng;
use rocket::http::Status;
use rocket::local::asynchronous::Client;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use validator_client::nymd::cosmwasm_client::types::ExecuteResult;

const TEST_COIN_DENOM: &str = "unym";
const TEST_REWARDING_VALIDATOR_ADDRESS: &str = "n19lc9u84cz0yz3fww5283nucc9yvr8gsjmgeul0";

#[derive(Clone, Debug)]
struct DummyClient {
    validator_address: AccountId,
    tx_db: Arc<RwLock<HashMap<String, TxResponse>>>,
    proposal_db: Arc<RwLock<HashMap<u64, ProposalResponse>>>,
    spent_credential_db: Arc<RwLock<HashMap<String, SpendCredentialResponse>>>,
}

impl DummyClient {
    pub fn new(
        validator_address: AccountId,
        tx_db: &Arc<RwLock<HashMap<String, TxResponse>>>,
        proposal_db: &Arc<RwLock<HashMap<u64, ProposalResponse>>>,
        spent_credential_db: &Arc<RwLock<HashMap<String, SpendCredentialResponse>>>,
    ) -> Self {
        let tx_db = Arc::clone(tx_db);
        let proposal_db = Arc::clone(proposal_db);
        let spent_credential_db = Arc::clone(spent_credential_db);
        Self {
            validator_address,
            tx_db,
            proposal_db,
            spent_credential_db,
        }
    }
}

#[async_trait]
impl super::client::Client for DummyClient {
    async fn address(&self) -> AccountId {
        self.validator_address.clone()
    }

    async fn get_tx(&self, tx_hash: &str) -> Result<TxResponse> {
        self.tx_db
            .read()
            .unwrap()
            .get(tx_hash)
            .cloned()
            .ok_or(CoconutError::TxHashParseError)
    }

    async fn get_proposal(&self, proposal_id: u64) -> Result<ProposalResponse> {
        self.proposal_db
            .read()
            .unwrap()
            .get(&proposal_id)
            .cloned()
            .ok_or(CoconutError::IncorrectProposal {
                reason: String::from("proposal not found"),
            })
    }

    async fn get_spent_credential(
        &self,
        blinded_serial_number: String,
    ) -> Result<SpendCredentialResponse> {
        self.spent_credential_db
            .read()
            .unwrap()
            .get(&blinded_serial_number)
            .cloned()
            .ok_or(CoconutError::InvalidCredentialStatus {
                status: String::from("spent credential not found"),
            })
    }

    async fn get_current_epoch_state(&self) -> Result<EpochState> {
        todo!()
    }

    async fn get_self_registered_dealer_details(&self) -> Result<DealerDetailsResponse> {
        todo!()
    }

    async fn get_current_dealers(&self) -> Result<Vec<DealerDetails>> {
        todo!()
    }

    async fn get_dealings(&self, idx: usize) -> Result<Vec<ContractDealing>> {
        todo!()
    }

    async fn vote_proposal(
        &self,
        proposal_id: u64,
        vote_yes: bool,
        _fee: Option<Fee>,
    ) -> Result<()> {
        if let Some(proposal) = self.proposal_db.write().unwrap().get_mut(&proposal_id) {
            if vote_yes {
                proposal.status = cw3::Status::Passed;
            } else {
                proposal.status = cw3::Status::Rejected;
            }
        }
        Ok(())
    }

    async fn register_dealer(
        &self,
        bte_key: EncodedBTEPublicKeyWithProof,
    ) -> Result<ExecuteResult> {
        todo!()
    }

    async fn submit_dealing(&self, dealing_bytes: ContractSafeBytes) -> Result<ExecuteResult> {
        todo!()
    }
}

#[derive(Clone, Debug)]
pub struct DummyCommunicationChannel {
    aggregated_verification_key: VerificationKey,
}

impl DummyCommunicationChannel {
    pub fn new(aggregated_verification_key: VerificationKey) -> Self {
        DummyCommunicationChannel {
            aggregated_verification_key,
        }
    }
}

#[async_trait]
impl super::comm::APICommunicationChannel for DummyCommunicationChannel {
    async fn aggregated_verification_key(&self) -> Result<VerificationKey> {
        Ok(self.aggregated_verification_key.clone())
    }
}

pub fn tx_entry_fixture(tx_hash: &str) -> TxResponse {
    TxResponse {
        hash: Hash::from_str(tx_hash).unwrap(),
        height: Default::default(),
        index: 0,
        tx_result: DeliverTx {
            code: Default::default(),
            data: Default::default(),
            log: Default::default(),
            info: Default::default(),
            gas_wanted: Default::default(),
            gas_used: Default::default(),
            events: vec![],
            codespace: Default::default(),
        },
        tx: vec![].into(),
        proof: None,
    }
}

async fn check_signer_verif_key(key_pair: KeyPair) {
    let verification_key = key_pair.verification_key();

    let mut db_dir = std::env::temp_dir();
    db_dir.push(&verification_key.to_bs58()[..8]);
    let storage = ValidatorApiStorage::init(db_dir).await.unwrap();
    let nymd_client = DummyClient::new(
        AccountId::from_str(TEST_REWARDING_VALIDATOR_ADDRESS).unwrap(),
        &Arc::new(RwLock::new(HashMap::new())),
        &Arc::new(RwLock::new(HashMap::new())),
        &Arc::new(RwLock::new(HashMap::new())),
    );
    let comm_channel = DummyCommunicationChannel::new(key_pair.verification_key());
    let staged_key_pair = crate::coconut::KeyPair::new();
    staged_key_pair.set(key_pair).await;

    let rocket = rocket::build().attach(InternalSignRequest::stage(
        nymd_client,
        TEST_COIN_DENOM.to_string(),
        staged_key_pair,
        comm_channel,
        storage,
    ));

    let client = Client::tracked(rocket)
        .await
        .expect("valid rocket instance");

    let response = client
        .get(format!(
            "/{}/{}/{}/{}",
            API_VERSION, COCONUT_ROUTES, BANDWIDTH, COCONUT_VERIFICATION_KEY
        ))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);

    // This is a more direct way, but there's a bug which makes it hang https://github.com/SergioBenitez/Rocket/issues/1893
    // assert!(response
    //     .into_json::<BlindedSignatureResponse>()
    //     .await
    //     .is_some());
    let verification_key_response =
        serde_json::from_str::<VerificationKeyResponse>(&response.into_string().await.unwrap())
            .unwrap();
    assert_eq!(verification_key_response.key, verification_key);
}

#[tokio::test]
async fn multiple_verification_key() {
    let params = Parameters::new(4).unwrap();
    let num_authorities = 4;

    let key_pairs = ttp_keygen(&params, num_authorities, num_authorities).unwrap();
    for key_pair in key_pairs.into_iter() {
        check_signer_verif_key(key_pair).await;
    }
}

#[tokio::test]
async fn signed_before() {
    let tx_hash =
        Hash::from_str("6B27412050B823E58BB38447D7870BBC8CBE3C51C905BEA89D459ACCDA80A00E").unwrap();
    let tx_entry = tx_entry_fixture(&tx_hash.to_string());
    let signature = String::from(
        "2DHbEZ6pzToGpsAXJrqJi7Wj1pAXeT18283q2YEEyNH5gTymwRozWBdja6SMAVt1dyYmUnM4ZNhsJ4wxZyGh4Z6J",
    );

    let params = Parameters::new(4).unwrap();
    let mut rng = OsRng;
    let voucher = BandwidthVoucher::new(
        &params,
        "1234".to_string(),
        VOUCHER_INFO.to_string(),
        tx_hash,
        identity::PrivateKey::from_base58_string(
            identity::KeyPair::new(&mut rng)
                .private_key()
                .to_base58_string(),
        )
        .unwrap(),
        encryption::PrivateKey::from_bytes(
            &encryption::KeyPair::new(&mut rng).private_key().to_bytes(),
        )
        .unwrap(),
    );
    let (_, blind_sign_req) = prepare_blind_sign(
        &params,
        &voucher.get_private_attributes(),
        &voucher.get_public_attributes(),
    )
    .unwrap();

    let key_pair = ttp_keygen(&params, 1, 1).unwrap().remove(0);
    let mut db_dir = std::env::temp_dir();
    db_dir.push(&key_pair.verification_key().to_bs58()[..8]);
    let storage = ValidatorApiStorage::init(db_dir).await.unwrap();
    let tx_db = Arc::new(RwLock::new(HashMap::new()));
    tx_db
        .write()
        .unwrap()
        .insert(tx_hash.to_string(), tx_entry.clone());
    let nymd_client = DummyClient::new(
        AccountId::from_str(TEST_REWARDING_VALIDATOR_ADDRESS).unwrap(),
        &tx_db,
        &Arc::new(RwLock::new(HashMap::new())),
        &Arc::new(RwLock::new(HashMap::new())),
    );
    let comm_channel = DummyCommunicationChannel::new(key_pair.verification_key());
    let staged_key_pair = crate::coconut::KeyPair::new();
    staged_key_pair.set(key_pair).await;

    let rocket = rocket::build().attach(InternalSignRequest::stage(
        nymd_client,
        TEST_COIN_DENOM.to_string(),
        staged_key_pair,
        comm_channel,
        storage.clone(),
    ));
    let client = Client::tracked(rocket)
        .await
        .expect("valid rocket instance");

    let request_body = BlindSignRequestBody::new(
        &blind_sign_req,
        tx_hash.to_string(),
        signature.clone(),
        &voucher.get_public_attributes(),
        voucher.get_public_attributes_plain(),
        4,
    );

    let encrypted_signature = vec![1, 2, 3, 4];
    let remote_key = [42; 32];
    let expected_response = BlindedSignatureResponse::new(encrypted_signature, remote_key);
    storage
        .insert_blinded_signature_response(
            &tx_hash.to_string(),
            &expected_response.to_base58_string(),
        )
        .await
        .unwrap();

    let response = client
        .post(format!(
            "/{}/{}/{}/{}",
            API_VERSION, COCONUT_ROUTES, BANDWIDTH, COCONUT_BLIND_SIGN
        ))
        .json(&request_body)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);

    // This is a more direct way, but there's a bug which makes it hang https://github.com/SergioBenitez/Rocket/issues/1893
    // let blinded_signature_response = response
    //     .into_json::<BlindedSignatureResponse>()
    //     .await
    //     .unwrap();
    let blinded_signature_response =
        serde_json::from_str::<BlindedSignatureResponse>(&response.into_string().await.unwrap())
            .unwrap();
    assert_eq!(
        blinded_signature_response.to_bytes(),
        expected_response.to_bytes()
    );
}

#[tokio::test]
async fn state_functions() {
    let nymd_client = DummyClient::new(
        AccountId::from_str(TEST_REWARDING_VALIDATOR_ADDRESS).unwrap(),
        &Arc::new(RwLock::new(HashMap::new())),
        &Arc::new(RwLock::new(HashMap::new())),
        &Arc::new(RwLock::new(HashMap::new())),
    );
    let params = Parameters::new(4).unwrap();
    let key_pair = ttp_keygen(&params, 1, 1).unwrap().remove(0);
    let mut db_dir = std::env::temp_dir();
    db_dir.push(&key_pair.verification_key().to_bs58()[..8]);
    let storage = ValidatorApiStorage::init(db_dir).await.unwrap();
    let comm_channel = DummyCommunicationChannel::new(key_pair.verification_key());
    let staged_key_pair = crate::coconut::KeyPair::new();
    staged_key_pair.set(key_pair).await;
    let state = State::new(
        nymd_client,
        TEST_COIN_DENOM.to_string(),
        staged_key_pair,
        comm_channel,
        storage.clone(),
    );

    let tx_hash = String::from("6B27412050B823E58BB38447D7870BBC8CBE3C51C905BEA89D459ACCDA80A00E");
    assert!(state.signed_before(&tx_hash).await.unwrap().is_none());

    let encrypted_signature = vec![1, 2, 3, 4];
    let remote_key = [42; 32];
    let expected_response = BlindedSignatureResponse::new(encrypted_signature, remote_key);
    storage
        .insert_blinded_signature_response(&tx_hash, &expected_response.to_base58_string())
        .await
        .unwrap();
    assert_eq!(
        state
            .signed_before(&tx_hash)
            .await
            .unwrap()
            .unwrap()
            .to_bytes(),
        expected_response.to_bytes()
    );

    let encryption_keypair = crypto::asymmetric::encryption::KeyPair::new(&mut OsRng);
    let blinded_signature = BlindedSignature::from_bytes(&[
        183, 217, 166, 113, 40, 123, 74, 25, 72, 31, 136, 19, 125, 95, 217, 228, 96, 113, 25, 240,
        12, 102, 125, 11, 174, 20, 216, 82, 192, 71, 27, 194, 48, 20, 17, 95, 243, 179, 82, 21, 57,
        143, 101, 19, 22, 186, 147, 13, 147, 238, 39, 119, 15, 36, 251, 131, 250, 38, 185, 113,
        187, 40, 227, 107, 134, 190, 123, 183, 126, 176, 226, 173, 147, 137, 17, 175, 13, 115, 78,
        222, 119, 93, 146, 116, 229, 0, 152, 51, 232, 2, 102, 204, 147, 202, 254, 243,
    ])
    .unwrap();
    // Check that the new payload is not stored if there was already something signed for tx_hash
    assert_eq!(
        state
            .encrypt_and_store(
                &tx_hash,
                encryption_keypair.public_key(),
                &blinded_signature,
            )
            .await
            .unwrap()
            .to_bytes(),
        expected_response.to_bytes()
    );

    // And use a new hash to store a new signature
    let tx_hash = String::from("97D64C38D6601B1F0FD3A82E20D252685CB7A210AFB0261018590659AB82B0BF");
    let response = state
        .encrypt_and_store(
            &tx_hash,
            encryption_keypair.public_key(),
            &blinded_signature,
        )
        .await
        .unwrap();
    let remote_key =
        crypto::asymmetric::encryption::PublicKey::from_bytes(&response.remote_key).unwrap();

    let encryption_key = recompute_shared_key::<
        ValidatorApiCredentialEncryptionAlgorithm,
        ValidatorApiCredentialHkdfAlgorithm,
    >(&remote_key, encryption_keypair.private_key());
    let zero_iv = stream_cipher::zero_iv::<ValidatorApiCredentialEncryptionAlgorithm>();
    let blinded_signature_bytes = stream_cipher::decrypt::<ValidatorApiCredentialEncryptionAlgorithm>(
        &encryption_key,
        &zero_iv,
        &response.encrypted_signature,
    );

    let received_blinded_signature =
        BlindedSignature::from_bytes(&blinded_signature_bytes).unwrap();
    assert_eq!(
        blinded_signature.to_bytes(),
        received_blinded_signature.to_bytes()
    );

    // Check that the same value for tx_hash is returned

    let other_signature = BlindedSignature::from_bytes(&[
        183, 217, 166, 113, 40, 123, 74, 25, 72, 31, 136, 19, 125, 95, 217, 228, 96, 113, 25, 240,
        12, 102, 125, 11, 174, 20, 216, 82, 192, 71, 27, 194, 48, 20, 17, 95, 243, 179, 82, 21, 57,
        143, 101, 19, 22, 186, 147, 13, 131, 236, 38, 138, 192, 235, 169, 142, 176, 118, 153, 238,
        141, 91, 94, 139, 168, 214, 17, 250, 96, 206, 139, 89, 139, 87, 31, 8, 106, 171, 8, 140,
        201, 158, 18, 152, 24, 98, 153, 170, 141, 35, 190, 200, 19, 148, 71, 197,
    ])
    .unwrap();
    assert_eq!(
        state
            .encrypt_and_store(&tx_hash, encryption_keypair.public_key(), &other_signature,)
            .await
            .unwrap()
            .to_bytes(),
        response.to_bytes()
    );
}

#[tokio::test]
async fn blind_sign_correct() {
    let tx_hash =
        Hash::from_str("7C41AF8266D91DE55E1C8F4712E6A952A165ED3D8C27C7B00428CBD0DE00A52B").unwrap();

    let params = Parameters::new(4).unwrap();
    let mut rng = OsRng;
    let voucher = BandwidthVoucher::new(
        &params,
        "1234".to_string(),
        VOUCHER_INFO.to_string(),
        tx_hash,
        identity::PrivateKey::from_base58_string(
            identity::KeyPair::new(&mut rng)
                .private_key()
                .to_base58_string(),
        )
        .unwrap(),
        encryption::PrivateKey::from_bytes(
            &encryption::KeyPair::new(&mut rng).private_key().to_bytes(),
        )
        .unwrap(),
    );

    let key_pair = ttp_keygen(&params, 1, 1).unwrap().remove(0);
    let mut db_dir = std::env::temp_dir();
    db_dir.push(&key_pair.verification_key().to_bs58()[..8]);
    let storage = ValidatorApiStorage::init(db_dir).await.unwrap();
    let tx_db = Arc::new(RwLock::new(HashMap::new()));

    let mut tx_entry = tx_entry_fixture(&tx_hash.to_string());
    tx_entry.tx_result.events.push(Event {
        type_str: format!("wasm-{}", DEPOSITED_FUNDS_EVENT_TYPE),
        attributes: vec![],
    });
    tx_entry.tx_result.events.get_mut(0).unwrap().attributes = vec![
        Tag {
            key: DEPOSIT_VALUE.parse().unwrap(),
            value: "1234".parse().unwrap(),
        },
        Tag {
            key: DEPOSIT_INFO.parse().unwrap(),
            value: VOUCHER_INFO.parse().unwrap(),
        },
        Tag {
            key: DEPOSIT_IDENTITY_KEY.parse().unwrap(),
            value: "64auwDkWan7R8yH1Mwe9dS4qXgrDBCUNDg3Q4KFnd2P5"
                .parse()
                .unwrap(),
        },
        Tag {
            key: DEPOSIT_ENCRYPTION_KEY.parse().unwrap(),
            value: "HxnTpWTkgigSTAysVKLE8pEiUULHdTT1BxFfzfJvQRi6"
                .parse()
                .unwrap(),
        },
    ];
    tx_db
        .write()
        .unwrap()
        .insert(tx_hash.to_string(), tx_entry.clone());
    let nymd_client = DummyClient::new(
        AccountId::from_str(TEST_REWARDING_VALIDATOR_ADDRESS).unwrap(),
        &tx_db,
        &Arc::new(RwLock::new(HashMap::new())),
        &Arc::new(RwLock::new(HashMap::new())),
    );
    let comm_channel = DummyCommunicationChannel::new(key_pair.verification_key());
    let staged_key_pair = crate::coconut::KeyPair::new();
    staged_key_pair.set(key_pair).await;

    let rocket = rocket::build().attach(InternalSignRequest::stage(
        nymd_client,
        TEST_COIN_DENOM.to_string(),
        staged_key_pair,
        comm_channel,
        storage.clone(),
    ));
    let client = Client::tracked(rocket)
        .await
        .expect("valid rocket instance");

    // hard-coded values, that generate a correct signature
    let blind_sign_req = BlindSignRequest::from_bytes(&[
        176, 113, 19, 237, 218, 252, 113, 20, 225, 238, 59, 88, 217, 45, 233, 178, 65, 28, 242, 0,
        222, 48, 110, 216, 26, 111, 51, 235, 61, 74, 200, 15, 130, 245, 45, 170, 155, 190, 156, 77,
        180, 142, 29, 63, 15, 224, 150, 31, 139, 24, 65, 175, 143, 153, 11, 203, 33, 16, 152, 22,
        221, 203, 99, 233, 208, 142, 161, 194, 46, 227, 177, 96, 119, 30, 175, 69, 104, 14, 2, 191,
        26, 94, 30, 165, 15, 28, 40, 176, 1, 78, 253, 79, 20, 137, 102, 74, 2, 0, 0, 0, 0, 0, 0, 0,
        131, 133, 112, 115, 53, 98, 58, 166, 240, 70, 185, 210, 203, 12, 114, 66, 180, 38, 139, 12,
        187, 45, 250, 201, 68, 102, 159, 172, 218, 124, 151, 23, 172, 18, 216, 122, 246, 7, 185,
        76, 20, 167, 123, 122, 152, 241, 175, 226, 176, 8, 170, 70, 140, 252, 36, 130, 67, 204,
        111, 116, 107, 92, 200, 77, 252, 31, 138, 18, 10, 215, 165, 243, 95, 199, 193, 61, 200,
        187, 22, 198, 109, 213, 145, 71, 171, 132, 174, 68, 105, 248, 0, 115, 50, 55, 199, 84, 67,
        16, 125, 216, 250, 154, 115, 174, 9, 206, 44, 88, 63, 163, 124, 10, 239, 64, 158, 191, 27,
        169, 177, 194, 223, 142, 202, 206, 189, 122, 123, 91, 171, 15, 40, 192, 148, 75, 174, 24,
        116, 229, 127, 170, 110, 183, 151, 2, 118, 168, 22, 113, 87, 237, 91, 228, 249, 120, 114,
        255, 53, 175, 245, 39, 2, 0, 0, 0, 0, 0, 0, 0, 225, 45, 230, 25, 62, 202, 96, 166, 171,
        241, 206, 137, 254, 51, 154, 255, 122, 130, 107, 54, 5, 206, 207, 120, 193, 214, 64, 10,
        111, 195, 86, 55, 201, 36, 10, 18, 154, 158, 183, 87, 185, 59, 228, 89, 134, 193, 217, 188,
        64, 164, 249, 21, 248, 20, 207, 58, 31, 10, 19, 176, 246, 150, 45, 48, 2, 0, 0, 0, 0, 0, 0,
        0, 173, 60, 65, 209, 100, 114, 138, 186, 158, 150, 109, 230, 111, 86, 101, 72, 194, 237,
        173, 195, 139, 175, 238, 25, 169, 18, 188, 75, 77, 54, 111, 20, 115, 235, 195, 2, 123, 133,
        164, 81, 15, 45, 11, 84, 139, 38, 8, 224, 197, 181, 95, 147, 49, 77, 193, 207, 52, 141,
        195, 195, 66, 137, 17, 32,
    ])
    .unwrap();
    let request_body = BlindSignRequestBody::new(
        &blind_sign_req,
        tx_hash.to_string(),
        "gSFgpma5GAVMcsmZwKieqGNHNd3dPzcfa8eT2Qn2LoBccSeyiJdphREbNrkuh5XWxMe2hUsranaYzLro48L9Qhd"
            .to_string(),
        &voucher.get_public_attributes(),
        voucher.get_public_attributes_plain(),
        4,
    );

    let response = client
        .post(format!(
            "/{}/{}/{}/{}",
            API_VERSION, COCONUT_ROUTES, BANDWIDTH, COCONUT_BLIND_SIGN
        ))
        .json(&request_body)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    // This is a more direct way, but there's a bug which makes it hang https://github.com/SergioBenitez/Rocket/issues/1893
    // assert!(response.into_json::<BlindedSignatureResponse>().is_some());
    let blinded_signature_response =
        serde_json::from_str::<BlindedSignatureResponse>(&response.into_string().await.unwrap());
    assert!(blinded_signature_response.is_ok());
}

#[tokio::test]
async fn signature_test() {
    let tx_hash = String::from("7C41AF8266D91DE55E1C8F4712E6A952A165ED3D8C27C7B00428CBD0DE00A52B");
    let params = Parameters::new(4).unwrap();

    let key_pair = ttp_keygen(&params, 1, 1).unwrap().remove(0);
    let mut db_dir = std::env::temp_dir();
    db_dir.push(&key_pair.verification_key().to_bs58()[..8]);
    let storage = ValidatorApiStorage::init(db_dir).await.unwrap();
    let nymd_client = DummyClient::new(
        AccountId::from_str(TEST_REWARDING_VALIDATOR_ADDRESS).unwrap(),
        &Arc::new(RwLock::new(HashMap::new())),
        &Arc::new(RwLock::new(HashMap::new())),
        &Arc::new(RwLock::new(HashMap::new())),
    );
    let comm_channel = DummyCommunicationChannel::new(key_pair.verification_key());
    let staged_key_pair = crate::coconut::KeyPair::new();
    staged_key_pair.set(key_pair).await;

    let rocket = rocket::build().attach(InternalSignRequest::stage(
        nymd_client,
        TEST_COIN_DENOM.to_string(),
        staged_key_pair,
        comm_channel,
        storage.clone(),
    ));
    let client = Client::tracked(rocket)
        .await
        .expect("valid rocket instance");

    let response = client
        .post(format!(
            "/{}/{}/{}/{}",
            API_VERSION, COCONUT_ROUTES, BANDWIDTH, COCONUT_PARTIAL_BANDWIDTH_CREDENTIAL
        ))
        .json(&tx_hash)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::BadRequest);
    assert_eq!(
        response.into_string().await.unwrap(),
        CoconutError::NoSignature.to_string()
    );

    let encrypted_signature = vec![1, 2, 3, 4];
    let remote_key = [42; 32];
    let expected_response = BlindedSignatureResponse::new(encrypted_signature, remote_key);
    storage
        .insert_blinded_signature_response(&tx_hash, &expected_response.to_base58_string())
        .await
        .unwrap();
    let response = client
        .post(format!(
            "/{}/{}/{}/{}",
            API_VERSION, COCONUT_ROUTES, BANDWIDTH, COCONUT_PARTIAL_BANDWIDTH_CREDENTIAL
        ))
        .json(&tx_hash)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    // This is a more direct way, but there's a bug which makes it hang https://github.com/SergioBenitez/Rocket/issues/1893
    // let blinded_signature_response = response
    //     .into_json::<BlindedSignatureResponse>()
    //     .await
    //     .unwrap();
    let blinded_signature_response =
        serde_json::from_str::<BlindedSignatureResponse>(&response.into_string().await.unwrap())
            .unwrap();
    assert_eq!(
        blinded_signature_response.to_bytes(),
        expected_response.to_bytes()
    );
}

#[tokio::test]
async fn get_cosmos_address() {
    let validator_address = AccountId::from_str(TEST_REWARDING_VALIDATOR_ADDRESS).unwrap();
    let nymd_client = DummyClient::new(
        validator_address.clone(),
        &Arc::new(RwLock::new(HashMap::new())),
        &Arc::new(RwLock::new(HashMap::new())),
        &Arc::new(RwLock::new(HashMap::new())),
    );
    let mut db_dir = std::env::temp_dir();
    let key_pair = ttp_keygen(&Parameters::new(4).unwrap(), 1, 1)
        .unwrap()
        .remove(0);
    db_dir.push(&key_pair.verification_key().to_bs58()[..8]);
    let storage = ValidatorApiStorage::init(db_dir).await.unwrap();
    let comm_channel = DummyCommunicationChannel::new(key_pair.verification_key());
    let staged_key_pair = crate::coconut::KeyPair::new();
    staged_key_pair.set(key_pair).await;
    let rocket = rocket::build().attach(InternalSignRequest::stage(
        nymd_client,
        TEST_COIN_DENOM.to_string(),
        staged_key_pair,
        comm_channel,
        storage.clone(),
    ));
    let client = Client::tracked(rocket)
        .await
        .expect("valid rocket instance");

    let response = client
        .get(format!(
            "/{}/{}/{}/{}",
            API_VERSION, COCONUT_ROUTES, BANDWIDTH, COCONUT_COSMOS_ADDRESS
        ))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    let cosmos_addr_response =
        serde_json::from_str::<CosmosAddressResponse>(&response.into_string().await.unwrap())
            .unwrap();
    assert_eq!(validator_address, cosmos_addr_response.addr);
}

#[tokio::test]
async fn verification_of_bandwidth_credential() {
    // Setup variables
    let validator_address = AccountId::from_str(TEST_REWARDING_VALIDATOR_ADDRESS).unwrap();
    let proposal_db = Arc::new(RwLock::new(HashMap::new()));
    let spent_credential_db = Arc::new(RwLock::new(HashMap::new()));
    let nymd_client = DummyClient::new(
        validator_address.clone(),
        &Arc::new(RwLock::new(HashMap::new())),
        &proposal_db,
        &spent_credential_db,
    );
    let mut db_dir = std::env::temp_dir();
    let params = Parameters::new(4).unwrap();
    let mut key_pairs = ttp_keygen(&params, 1, 1).unwrap();
    let voucher_value = 1234u64;
    let voucher_info = "voucher info";
    let public_attributes = vec![
        hash_to_scalar(voucher_value.to_string()),
        hash_to_scalar(voucher_info),
    ];
    let indices: Vec<u64> = key_pairs
        .iter()
        .enumerate()
        .map(|(idx, _)| (idx + 1) as u64)
        .collect();
    let theta =
        theta_from_keys_and_attributes(&params, &key_pairs, &indices, &public_attributes).unwrap();
    let key_pair = key_pairs.remove(0);
    db_dir.push(&key_pair.verification_key().to_bs58()[..8]);
    let storage1 = ValidatorApiStorage::init(db_dir).await.unwrap();
    let comm_channel = DummyCommunicationChannel::new(key_pair.verification_key());
    let staged_key_pair = crate::coconut::KeyPair::new();
    staged_key_pair.set(key_pair).await;
    let rocket = rocket::build().attach(InternalSignRequest::stage(
        nymd_client.clone(),
        TEST_COIN_DENOM.to_string(),
        staged_key_pair,
        comm_channel.clone(),
        storage1.clone(),
    ));

    let client = Client::tracked(rocket)
        .await
        .expect("valid rocket instance");

    let credential = Credential::new(4, theta.clone(), voucher_value, voucher_info.to_string());
    let proposal_id = 42;
    // The address is not used, so we can use a duplicate
    let gateway_cosmos_addr = validator_address.clone();
    let req =
        VerifyCredentialBody::new(credential.clone(), proposal_id, gateway_cosmos_addr.clone());

    // Test endpoint with not proposal for the proposal id
    let response = client
        .post(format!(
            "/{}/{}/{}/{}",
            API_VERSION, COCONUT_ROUTES, BANDWIDTH, COCONUT_VERIFY_BANDWIDTH_CREDENTIAL
        ))
        .json(&req)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::BadRequest);
    assert_eq!(
        response.into_string().await.unwrap(),
        CoconutError::IncorrectProposal {
            reason: "proposal not found".to_string()
        }
        .to_string()
    );

    let mut proposal = ProposalResponse {
        id: proposal_id,
        title: String::new(),
        description: String::from("25mnnoCcUfeizfC85avvroFg2prpEZBgJbJM2SLtkgyyUkoAU3cqJiqWmg8cMHEPjfFf5sQF92SMAM2vbEoLZvUjenvXhadTLdA4TqMYArJpihyqirW2AhGoNehtcdcK5gnH"),
        msgs: vec![],
        status: cw3::Status::Open,
        expires: cw_utils::Expiration::Never {},
        threshold: cw_utils::ThresholdResponse::AbsolutePercentage {
            percentage: Decimal::from_ratio(2u32, 3u32),
            total_weight: 100,
        },
    };

    // Test the endpoint with a different blinded serial number in the description
    proposal_db
        .write()
        .unwrap()
        .insert(proposal_id, proposal.clone());
    let response = client
        .post(format!(
            "/{}/{}/{}/{}",
            API_VERSION, COCONUT_ROUTES, BANDWIDTH, COCONUT_VERIFY_BANDWIDTH_CREDENTIAL
        ))
        .json(&req)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::BadRequest);
    assert_eq!(
        response.into_string().await.unwrap(),
        CoconutError::IncorrectProposal {
            reason: "incorrect blinded serial number in description".to_string()
        }
        .to_string()
    );

    // Test the endpoint with no msg in the proposal action
    proposal.description = credential.blinded_serial_number();
    proposal_db
        .write()
        .unwrap()
        .insert(proposal_id, proposal.clone());
    let response = client
        .post(format!(
            "/{}/{}/{}/{}",
            API_VERSION, COCONUT_ROUTES, BANDWIDTH, COCONUT_VERIFY_BANDWIDTH_CREDENTIAL
        ))
        .json(&req)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::BadRequest);
    assert_eq!(
        response.into_string().await.unwrap(),
        CoconutError::IncorrectProposal {
            reason: "action is not to release funds".to_string()
        }
        .to_string()
    );

    // Test the endpoint without any credential recorded in the Coconut Bandwidth Contract
    let funds = Coin::new(voucher_value as u128, TEST_COIN_DENOM);
    let msg = coconut_bandwidth_contract_common::msg::ExecuteMsg::ReleaseFunds {
        funds: funds.clone().into(),
    };
    let cosmos_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: String::new(),
        msg: to_binary(&msg).unwrap(),
        funds: vec![],
    });
    proposal.msgs = vec![cosmos_msg];
    proposal_db
        .write()
        .unwrap()
        .insert(proposal_id, proposal.clone());
    let response = client
        .post(format!(
            "/{}/{}/{}/{}",
            API_VERSION, COCONUT_ROUTES, BANDWIDTH, COCONUT_VERIFY_BANDWIDTH_CREDENTIAL
        ))
        .json(&req)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::BadRequest);
    assert_eq!(
        response.into_string().await.unwrap(),
        CoconutError::InvalidCredentialStatus {
            status: "spent credential not found".to_string()
        }
        .to_string()
    );

    spent_credential_db.write().unwrap().insert(
        credential.blinded_serial_number(),
        SpendCredentialResponse::new(None),
    );
    let response = client
        .post(format!(
            "/{}/{}/{}/{}",
            API_VERSION, COCONUT_ROUTES, BANDWIDTH, COCONUT_VERIFY_BANDWIDTH_CREDENTIAL
        ))
        .json(&req)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::BadRequest);
    assert_eq!(
        response.into_string().await.unwrap(),
        CoconutError::InvalidCredentialStatus {
            status: "Inexistent".to_string()
        }
        .to_string()
    );

    // Test the endpoint with a credential that doesn't verify correctly
    let mut spent_credential = SpendCredential::new(
        funds.clone().into(),
        credential.blinded_serial_number(),
        Addr::unchecked("unimportant"),
    );
    spent_credential_db.write().unwrap().insert(
        credential.blinded_serial_number(),
        SpendCredentialResponse::new(Some(spent_credential.clone())),
    );
    let bad_credential = Credential::new(
        4,
        theta.clone(),
        voucher_value,
        String::from("bad voucher info"),
    );
    let bad_req =
        VerifyCredentialBody::new(bad_credential, proposal_id, gateway_cosmos_addr.clone());
    let response = client
        .post(format!(
            "/{}/{}/{}/{}",
            API_VERSION, COCONUT_ROUTES, BANDWIDTH, COCONUT_VERIFY_BANDWIDTH_CREDENTIAL
        ))
        .json(&bad_req)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    let verify_credential_response =
        serde_json::from_str::<VerifyCredentialResponse>(&response.into_string().await.unwrap())
            .unwrap();
    assert!(!verify_credential_response.verification_result);
    assert_eq!(
        cw3::Status::Rejected,
        proposal_db
            .read()
            .unwrap()
            .get(&proposal_id)
            .unwrap()
            .status
    );

    // Test the endpoint with a proposal that has a different value for the funds to be released
    // then what's in the credential
    let funds = Coin::new((voucher_value + 10) as u128, TEST_COIN_DENOM);
    let msg = coconut_bandwidth_contract_common::msg::ExecuteMsg::ReleaseFunds {
        funds: funds.clone().into(),
    };
    let cosmos_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: String::new(),
        msg: to_binary(&msg).unwrap(),
        funds: vec![],
    });
    proposal.msgs = vec![cosmos_msg];
    proposal_db
        .write()
        .unwrap()
        .insert(proposal_id, proposal.clone());
    let response = client
        .post(format!(
            "/{}/{}/{}/{}",
            API_VERSION, COCONUT_ROUTES, BANDWIDTH, COCONUT_VERIFY_BANDWIDTH_CREDENTIAL
        ))
        .json(&req)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    let verify_credential_response =
        serde_json::from_str::<VerifyCredentialResponse>(&response.into_string().await.unwrap())
            .unwrap();
    assert!(!verify_credential_response.verification_result);
    assert_eq!(
        cw3::Status::Rejected,
        proposal_db
            .read()
            .unwrap()
            .get(&proposal_id)
            .unwrap()
            .status
    );

    // Test the endpoint with every dependency met
    let funds = Coin::new(voucher_value as u128, TEST_COIN_DENOM);
    let msg = coconut_bandwidth_contract_common::msg::ExecuteMsg::ReleaseFunds {
        funds: funds.clone().into(),
    };
    let cosmos_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: String::new(),
        msg: to_binary(&msg).unwrap(),
        funds: vec![],
    });
    proposal.msgs = vec![cosmos_msg];
    proposal_db
        .write()
        .unwrap()
        .insert(proposal_id, proposal.clone());
    let response = client
        .post(format!(
            "/{}/{}/{}/{}",
            API_VERSION, COCONUT_ROUTES, BANDWIDTH, COCONUT_VERIFY_BANDWIDTH_CREDENTIAL
        ))
        .json(&req)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    let verify_credential_response =
        serde_json::from_str::<VerifyCredentialResponse>(&response.into_string().await.unwrap())
            .unwrap();
    assert!(verify_credential_response.verification_result);
    assert_eq!(
        cw3::Status::Passed,
        proposal_db
            .read()
            .unwrap()
            .get(&proposal_id)
            .unwrap()
            .status
    );

    // Test the endpoint with the credential marked as Spent in the Coconut Bandwidth Contract
    spent_credential.mark_as_spent();
    spent_credential_db.write().unwrap().insert(
        credential.blinded_serial_number(),
        SpendCredentialResponse::new(Some(spent_credential)),
    );
    let response = client
        .post(format!(
            "/{}/{}/{}/{}",
            API_VERSION, COCONUT_ROUTES, BANDWIDTH, COCONUT_VERIFY_BANDWIDTH_CREDENTIAL
        ))
        .json(&req)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::BadRequest);
    assert_eq!(
        response.into_string().await.unwrap(),
        CoconutError::InvalidCredentialStatus {
            status: "Spent".to_string()
        }
        .to_string()
    );
}
