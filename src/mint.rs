use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::dhke::verify_message;
use crate::error::Error;
use crate::types::{
    self, BlindedMessage, CheckSpendableRequest, CheckSpendableResponse, MeltRequest, MeltResponse,
    PostMintResponse, Promise, SplitRequest, SplitResponse,
};
use crate::Amount;
use crate::{
    dhke::sign_message,
    keyset::{
        self,
        mint::{self, KeySet},
        PublicKey,
    },
    types::MintRequest,
};

pub struct Mint {
    pub active_keyset: KeySet,
    pub inactive_keysets: HashMap<String, mint::KeySet>,
    pub spent_secrets: HashSet<String>,
}

impl Mint {
    pub fn new(
        secret: &str,
        derivation_path: &str,
        inactive_keysets: HashMap<String, mint::KeySet>,
        spent_secrets: HashSet<String>,
        max_order: u8,
    ) -> Self {
        Self {
            active_keyset: keyset::mint::KeySet::generate(secret, derivation_path, max_order),
            inactive_keysets,
            spent_secrets,
        }
    }

    /// Retrieve the public keys of the active keyset for distribution to
    /// wallet clients
    pub fn active_keyset_pubkeys(&self) -> keyset::KeySet {
        keyset::KeySet::from(self.active_keyset.clone())
    }

    /// Return a list of all supported keysets
    pub fn keysets(&self) -> keyset::Response {
        let mut keysets: HashSet<_> = self.inactive_keysets.keys().cloned().collect();
        keysets.insert(self.active_keyset.id.clone());
        keyset::Response { keysets }
    }

    pub fn active_keyset(&self) -> keyset::mint::KeySet {
        self.active_keyset.clone()
    }

    pub fn keyset(&self, id: &str) -> Option<keyset::KeySet> {
        if &self.active_keyset.id == id {
            return Some(self.active_keyset.clone().into());
        }

        self.inactive_keysets.get(id).map(|k| k.clone().into())
    }

    pub fn process_mint_request(
        &mut self,
        mint_request: MintRequest,
    ) -> Result<PostMintResponse, Error> {
        let mut blind_signatures = Vec::with_capacity(mint_request.outputs.len());

        for blinded_message in mint_request.outputs {
            blind_signatures.push(self.blind_sign(&blinded_message)?);
        }

        Ok(PostMintResponse {
            promises: blind_signatures,
        })
    }

    fn blind_sign(&self, blinded_message: &BlindedMessage) -> Result<Promise, Error> {
        let BlindedMessage { amount, b } = blinded_message;

        let Some(key_pair) = self.active_keyset.keys.0.get(&amount.to_sat()) else {
            // No key for amount
            return Err(Error::AmountKey);
        };

        let c = sign_message(key_pair.secret_key.clone(), b.clone().into())?;

        Ok(Promise {
            amount: amount.clone(),
            c: c.into(),
            id: self.active_keyset.id.clone(),
        })
    }

    fn create_split_response(
        &self,
        amount: Amount,
        outputs: &[BlindedMessage],
    ) -> Result<SplitResponse, Error> {
        let mut target_total = Amount::ZERO;
        let mut change_total = Amount::ZERO;
        let mut target = Vec::with_capacity(outputs.len());
        let mut change = Vec::with_capacity(outputs.len());

        // Create sets of target and change amounts that we're looking for
        // in the outputs (blind messages). As we loop, take from those sets,
        // target amount first.
        for output in outputs {
            let signed = self.blind_sign(&output)?;

            // Accumulate outputs into the target (send) list
            if target_total + signed.amount <= amount {
                target_total += signed.amount;
                target.push(signed);
            } else {
                change_total += signed.amount;
                change.push(signed);
            }
        }

        Ok(SplitResponse {
            fst: change,
            snd: target,
        })
    }

    pub fn process_split_request(
        &mut self,
        split_request: SplitRequest,
    ) -> Result<SplitResponse, Error> {
        let proofs_total = split_request.proofs_amount();
        if proofs_total < split_request.amount {
            return Err(Error::Amount);
        }

        let output_total = split_request.output_amount();
        if output_total < split_request.amount {
            return Err(Error::Amount);
        }

        if proofs_total != output_total {
            return Err(Error::Amount);
        }

        let mut secrets = Vec::with_capacity(split_request.proofs.len());
        for proof in &split_request.proofs {
            secrets.push(self.verify_proof(proof)?);
        }

        let mut split_response =
            self.create_split_response(split_request.amount, &split_request.outputs)?;

        if split_response.target_amount() != split_request.amount {
            let mut outputs = split_request.outputs;
            outputs.reverse();
            split_response = self.create_split_response(split_request.amount, &outputs)?;
        }

        if split_response.target_amount() != split_request.amount {
            return Err(Error::OutputOrdering);
        }

        for secret in secrets {
            self.spent_secrets.insert(secret);
        }

        Ok(split_response)
    }

    fn verify_proof(&self, proof: &types::Proof) -> Result<String, Error> {
        if self.spent_secrets.contains(&proof.secret) {
            return Err(Error::TokenSpent);
        }

        let keyset = proof.id.as_ref().map_or_else(
            || &self.active_keyset,
            |id| {
                if let Some(keyset) = self.inactive_keysets.get(id) {
                    keyset
                } else {
                    &self.active_keyset
                }
            },
        );

        let Some(keypair) = keyset.keys.0.get(&proof.amount.to_sat()) else {
            return Err(Error::AmountKey);
        };

        verify_message(
            keypair.secret_key.to_owned(),
            proof.c.clone().into(),
            &proof.secret,
        )?;

        Ok(proof.secret.clone())
    }

    pub fn check_spendable(
        &self,
        check_spendable: &CheckSpendableRequest,
    ) -> Result<CheckSpendableResponse, Error> {
        let mut spendable = vec![];
        for proof in &check_spendable.proofs {
            spendable.push(self.spent_secrets.contains(&proof.secret))
        }

        Ok(CheckSpendableResponse { spendable })
    }

    pub fn verify_melt_request(&mut self, melt_request: &MeltRequest) -> Result<(), Error> {
        let proofs_total = melt_request.proofs_amount();

        // TODO: Fee reserve
        if proofs_total < melt_request.invoice_amount()? {
            return Err(Error::Amount);
        }

        let mut secrets = Vec::with_capacity(melt_request.proofs.len());
        for proof in &melt_request.proofs {
            secrets.push(self.verify_proof(&proof)?);
        }

        Ok(())
    }

    pub fn process_melt_request(
        &mut self,
        melt_request: &MeltRequest,
        preimage: &str,
        total_spent: Amount,
    ) -> Result<MeltResponse, Error> {
        let secrets = Vec::with_capacity(melt_request.proofs.len());
        for secret in secrets {
            self.spent_secrets.insert(secret);
        }

        let change_target = melt_request.proofs_amount() - total_spent;
        let amounts = change_target.split();
        let mut change = Vec::with_capacity(amounts.len());

        if let Some(outputs) = &melt_request.outputs {
            for (i, amount) in amounts.iter().enumerate() {
                let mut message = outputs[i].clone();

                message.amount = amount.clone();

                let signature = self.blind_sign(&message)?;
                change.push(signature)
            }
        }

        Ok(MeltResponse {
            paid: true,
            preimage: Some(preimage.to_string()),
            change: Some(change),
        })
    }
}

/// Proofs [NUT-00]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proof {
    /// Amount in satoshi
    pub amount: Option<Amount>,
    /// Secret message
    // #[serde(with = "crate::serde_utils::bytes_base64")]
    pub secret: String,
    /// Unblinded signature
    #[serde(rename = "C")]
    pub c: Option<PublicKey>,
    /// `Keyset id`
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// P2SHScript that specifies the spending condition for this Proof
    pub script: Option<String>,
}

/// List of proofs
pub type Proofs = Vec<Proof>;