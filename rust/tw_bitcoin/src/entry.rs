use crate::Result;
use bitcoin::absolute::{Height, LockTime, Time};
use bitcoin::address::{NetworkChecked, Payload};
use bitcoin::consensus::encode::Encodable;
use bitcoin::key::{TapTweak, TweakedKeyPair};
use bitcoin::taproot::{ControlBlock, TapLeafHash, TapNodeHash};
use bitcoin::{
    OutPoint, PubkeyHash, ScriptBuf, ScriptHash, Sequence, Transaction, TxIn, TxOut, Txid,
    WPubkeyHash, Witness,
};
use secp256k1::hashes::Hash;
use secp256k1::{KeyPair, Message, Secp256k1, XOnlyPublicKey};
use std::borrow::Cow;
use std::fmt::Display;
use tw_coin_entry::coin_context::CoinContext;
use tw_coin_entry::coin_entry::{CoinAddress, CoinEntry, PublicKeyBytes, SignatureBytes};
use tw_coin_entry::derivation::Derivation;
use tw_coin_entry::error::AddressResult;
use tw_coin_entry::modules::json_signer::JsonSigner;
use tw_coin_entry::modules::plan_builder::NoPlanBuilder;
use tw_coin_entry::prefix::NoPrefix;
use tw_keypair::tw::{PrivateKey, PublicKey};
use tw_misc::traits::ToBytesVec;
use tw_proto::BitcoinV2::Proto;
use tw_proto::Utxo::Proto as UtxoProto;
use tw_utxo::compiler::StandardBitcoinContext;

// Convenience aliases.
type ProtoOutputRecipient<'a> = Proto::mod_Output::OneOfto_recipient<'a>;
type ProtoBuilderType<'a> = Proto::mod_Builder::OneOftype_pb<'a>;
type ProtoPubkeyOrHash<'a> = Proto::mod_ToPublicKeyOrHash::OneOfto_address<'a>;
type ProtoInputVariant<'a> = Proto::mod_Input::OneOfvariant<'a>;
type ProtoInputBuilder<'a> = Proto::mod_Input::mod_InputVariant::OneOfvariant<'a>;

pub type PlaceHolderProto<'a> = tw_proto::Bitcoin::Proto::SigningInput<'a>;

pub struct PlaceHolder;

pub struct NoJsonSigner;

impl JsonSigner for NoJsonSigner {
    fn sign_json(
        &self,
        _coin: &dyn tw_coin_entry::coin_context::CoinContext,
        _input_json: &str,
        _key: &PrivateKey,
    ) -> tw_coin_entry::error::SigningResult<String> {
        todo!()
    }
}

pub struct BitcoinEntry;

pub struct Address(bitcoin::address::Address<NetworkChecked>);

impl Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl CoinAddress for Address {
    fn data(&self) -> tw_memory::Data {
        match &self.0.payload {
            Payload::PubkeyHash(hash) => hash.to_byte_array().into(),
            Payload::ScriptHash(hash) => hash.to_byte_array().into(),
            Payload::WitnessProgram(wp) => wp.program().as_bytes().into(),
            _ => todo!(), // Payload is non-exhaustive
        }
    }
}

// Todo: type should be unified.
fn convert_locktime(
    val: &Proto::mod_SigningInput::OneOflock_time,
) -> UtxoProto::mod_SigningInput::OneOflock_time {
    match val {
        Proto::mod_SigningInput::OneOflock_time::blocks(blocks) => {
            UtxoProto::mod_SigningInput::OneOflock_time::blocks(*blocks)
        },
        Proto::mod_SigningInput::OneOflock_time::seconds(seconds) => {
            UtxoProto::mod_SigningInput::OneOflock_time::seconds(*seconds)
        },
        Proto::mod_SigningInput::OneOflock_time::None => {
            UtxoProto::mod_SigningInput::OneOflock_time::None
        },
    }
}

impl CoinEntry for BitcoinEntry {
    type AddressPrefix = NoPrefix;
    type Address = Address;
    type SigningInput<'a> = Proto::SigningInput<'a>;
    type SigningOutput = Proto::SigningOutput<'static>;
    type PreSigningOutput = Proto::PreSigningOutput<'static>;

    // Optional modules:
    type JsonSigner = NoJsonSigner;
    type PlanBuilder = NoPlanBuilder;

    #[inline]
    fn parse_address(
        &self,
        _coin: &dyn CoinContext,
        _address: &str,
        _prefix: Option<Self::AddressPrefix>,
    ) -> AddressResult<Self::Address> {
        todo!()
    }

    #[inline]
    fn derive_address(
        &self,
        _coin: &dyn CoinContext,
        _public_key: PublicKey,
        _derivation: Derivation,
        _prefix: Option<Self::AddressPrefix>,
    ) -> AddressResult<Self::Address> {
        todo!()
    }

    #[inline]
    fn sign(&self, _coin: &dyn CoinContext, proto: Self::SigningInput<'_>) -> Self::SigningOutput {
        // TODO: Can we avoid cloning here?
        let pre_signed = self.preimage_hashes(_coin, proto.clone());
        // TODO: Check error

        let secp = Secp256k1::new();
        let keypair = KeyPair::from_seckey_slice(&secp, proto.private_key.as_ref()).unwrap();

        let mut signatures: Vec<SignatureBytes> = vec![];

        for (entry, utxo_in) in pre_signed
            .sighashes
            .iter()
            .zip(pre_signed.utxo_inputs.iter())
        {
            let sighash = Message::from_slice(entry.sighash.as_ref()).unwrap();

            match entry.signing_method {
                UtxoProto::SighashMethod::Legacy | UtxoProto::SighashMethod::Segwit => {
                    let sig = bitcoin::ecdsa::Signature {
                        sig: keypair.secret_key().sign_ecdsa(sighash),
                        // TODO
                        hash_ty: bitcoin::sighash::EcdsaSighashType::All,
                    };

                    signatures.push(sig.to_vec());
                },
                UtxoProto::SighashMethod::Taproot => {
                    // Any empty leaf hash implies P2TR key-path (balance transfer)
                    if utxo_in.leaf_hash.is_empty() {
                        // Tweak keypair for P2TR key-path (ie. zeroed Merkle root).
                        let tapped: TweakedKeyPair = keypair.tap_tweak(&secp, None);
                        let tweaked = KeyPair::from(tapped);

                        // Construct the Schnorr signature.
                        #[cfg(not(test))]
                        let schnorr = secp.sign_schnorr(&sighash, &tweaked);
                        #[cfg(test)]
                        // For tests, we disable the included randomness in order to create
                        // reproducible signatures. Randomness should ALWAYS be used in
                        // production.
                        let schnorr = secp.sign_schnorr_no_aux_rand(&sighash, &tweaked);

                        let sig = bitcoin::taproot::Signature {
                            sig: schnorr,
                            // TODO.
                            hash_ty: bitcoin::sighash::TapSighashType::All,
                        };

                        signatures.push(sig.to_vec());
                    }
                    // If it has a leaf hash, then it's a P2TR script-path (complex transaction)
                    else {
                        // We do not tweak the key here since we're passing on
                        // the "control block" when claiming, hence this signing
                        // process is simpler that P2TR key-path.
                        let sig = bitcoin::taproot::Signature {
                            sig: keypair.sign_schnorr(sighash),
                            // TODO.
                            hash_ty: bitcoin::sighash::TapSighashType::All,
                        };

                        signatures.push(sig.to_vec());
                    }
                },
            }
        }

        self.compile(_coin, proto, signatures, vec![])
    }

    #[inline]
    fn preimage_hashes(
        &self,
        _coin: &dyn CoinContext,
        proto: Proto::SigningInput<'_>,
    ) -> Self::PreSigningOutput {
        let utxo_outputs = process_recipients(&proto.outputs.clone());

        let total_spent: u64 = utxo_outputs.iter().map(|output| output.value).sum();

        let mut utxo_inputs = vec![];
        for input in proto.inputs.clone() {
            let mut leaf_hash = None;

            let (sighash_method, script_pubkey) = match &input.variant {
                ProtoInputVariant::builder(builder) => match &builder.variant {
                    ProtoInputBuilder::p2sh(_) => todo!(),
                    ProtoInputBuilder::p2pkh(pubkey_or_hash) => {
                        let pubkey_hash = pubkey_hash_from_proto(pubkey_or_hash).unwrap();

                        (
                            UtxoProto::SighashMethod::Legacy,
                            ScriptBuf::new_p2pkh(&pubkey_hash),
                        )
                    },
                    ProtoInputBuilder::p2wsh(_) => todo!(),
                    ProtoInputBuilder::p2wpkh(pubkey_or_hash) => {
                        let wpubkey_hash = witness_pubkey_hash_from_proto(pubkey_or_hash).unwrap();

                        (
                            UtxoProto::SighashMethod::Segwit,
                            ScriptBuf::new_v0_p2wpkh(&wpubkey_hash),
                        )
                    },
                    ProtoInputBuilder::p2tr_key_path(pubkey) => {
                        let pubkey = bitcoin::PublicKey::from_slice(pubkey.as_ref()).unwrap();
                        let xonly = XOnlyPublicKey::from(pubkey.inner);
                        let (output_key, _) = xonly.tap_tweak(&secp256k1::Secp256k1::new(), None);

                        (
                            UtxoProto::SighashMethod::Taproot,
                            ScriptBuf::new_v1_p2tr_tweaked(output_key),
                        )
                    },
                    ProtoInputBuilder::p2tr_script_path(complex) => {
                        let script_buf = ScriptBuf::from_bytes(complex.payload.to_vec());
                        leaf_hash = Some(TapLeafHash::from_script(
                            script_buf.as_script(),
                            bitcoin::taproot::LeafVersion::TapScript,
                        ));

                        (UtxoProto::SighashMethod::Taproot, script_buf)
                    },
                    ProtoInputBuilder::None => todo!(),
                },
                ProtoInputVariant::custom(custom) => {
                    todo!()
                },
                ProtoInputVariant::None => todo!(),
            };

            utxo_inputs.push(UtxoProto::TxIn {
                txid: input.txid.to_vec().into(),
                vout: input.vout,
                amount: input.amount,
                script_pubkey: script_pubkey.to_vec().into(),
                sighash_method,
                // TODO
                sighash: UtxoProto::SighashType::All,
                leaf_hash: leaf_hash
                    .map(|hash| hash.to_vec().into())
                    .unwrap_or_default(),
                one_prevout: input.one_prevout,
            });
        }

        let mut remaining = total_spent;
        // TODO: This logic can be combined with the processor above.
        match proto.input_selector {
            Proto::SelectorType::AutomaticAscending => {
                utxo_inputs.sort_by(|a, b| a.amount.partial_cmp(&b.amount).unwrap());

                let mut total_input_amount = 0;
                utxo_inputs = utxo_inputs
                    .into_iter()
                    .take_while(|input| {
                        total_input_amount += input.amount;
                        remaining = remaining.saturating_sub(input.amount);

                        remaining != 0
                    })
                    .collect();
            },
            // Do nothing.
            Proto::SelectorType::UseAll => {},
        }

        if remaining != 0 {
            // Error, insufficient funds.
            todo!()
        }

        let utxo_signing = UtxoProto::SigningInput {
            version: proto.version,
            lock_time: convert_locktime(&proto.lock_time),
            inputs: utxo_inputs.clone(),
            outputs: utxo_outputs.clone(),
        };

        let utxo_presigning = tw_utxo::compiler::Compiler::preimage_hashes(&utxo_signing);

        Proto::PreSigningOutput {
            error: 0,
            sighashes: utxo_presigning.sighashes,
            utxo_inputs: utxo_inputs.clone(),
            //utxo_inputs: Default::default(),
            utxo_outputs: utxo_outputs.clone(),
            //utxo_outputs: Default::default(),
        }
    }

    #[inline]
    fn compile(
        &self,
        _coin: &dyn CoinContext,
        proto: Proto::SigningInput<'_>,
        signatures: Vec<SignatureBytes>,
        _public_keys: Vec<PublicKeyBytes>,
    ) -> Self::SigningOutput {
        if proto.inputs.len() != signatures.len() {
            // Error
            todo!()
        }

        let mut utxo_input_claims: Vec<UtxoProto::TxInClaim> = vec![];

        // Generate claims for all the inputs.
        for (index, input) in proto.inputs.iter().enumerate() {
            let sig_slice = &signatures[index];

            let (script_sig, witness) = match &input.variant {
                ProtoInputVariant::builder(variant) => match &variant.variant {
                    ProtoInputBuilder::p2pkh(pubkey_or_hash) => {
                        let sig = bitcoin::ecdsa::Signature::from_slice(sig_slice).unwrap();
                        let pubkey_hash = pubkey_hash_from_proto(pubkey_or_hash).unwrap();

                        (
                            ScriptBuf::builder()
                                .push_slice(sig.serialize())
                                .push_slice(pubkey_hash)
                                .into_script(),
                            Witness::new(),
                        )
                    },
                    ProtoInputBuilder::p2wpkh(pubkey_or_hash) => {
                        let sig = bitcoin::ecdsa::Signature::from_slice(sig_slice).unwrap();
                        let wpubkey_hash = witness_pubkey_hash_from_proto(pubkey_or_hash).unwrap();

                        (ScriptBuf::new(), {
                            let mut w = Witness::new();
                            w.push(sig.serialize());
                            w.push(wpubkey_hash.as_byte_array());
                            w
                        })
                    },
                    ProtoInputBuilder::p2tr_key_path(_) => {
                        let sig = bitcoin::taproot::Signature::from_slice(sig_slice).unwrap();

                        (ScriptBuf::new(), {
                            let mut w = Witness::new();
                            w.push(sig.to_vec());
                            w
                        })
                    },
                    ProtoInputBuilder::p2tr_script_path(taproot) => {
                        let sig = bitcoin::taproot::Signature::from_slice(sig_slice).unwrap();
                        let control_block =
                            ControlBlock::decode(taproot.control_block.as_ref()).unwrap();

                        (ScriptBuf::new(), {
                            let mut w = Witness::new();
                            w.push(sig.to_vec());
                            w.push(taproot.payload.as_ref());
                            w.push(control_block.serialize());
                            w
                        })
                    },
                    _ => panic!(),
                },
                ProtoInputVariant::custom(custom) => (
                    ScriptBuf::from_bytes(custom.script_sig.to_vec()),
                    Witness::from_slice(&custom.witness_items),
                ),
                ProtoInputVariant::None => {
                    todo!()
                },
            };

            utxo_input_claims.push(UtxoProto::TxInClaim {
                txid: input.txid.clone(),
                vout: input.vout,
                // TODO
                sequence: 0,
                script_sig: script_sig.to_vec().into(),
                witness_items: witness
                    .to_vec()
                    .into_iter()
                    .map(Cow::Owned)
                    .collect::<Vec<Cow<_>>>(),
            });
        }

        // Process all the outputs.
        let utxo_outputs = process_recipients(&proto.outputs);

        let utxo_preserializtion = UtxoProto::PreSerialization {
            version: proto.version,
            // TODO:
            lock_time: UtxoProto::mod_PreSerialization::OneOflock_time::blocks(0),
            inputs: utxo_input_claims.clone(),
            outputs: utxo_outputs.clone(),
        };

        let utxo_serialized = tw_utxo::compiler::Compiler::compile(&utxo_preserializtion);

        // Prepare `Proto::TransactionInput` protobufs for signing output.
        let mut proto_inputs = vec![];
        for input in utxo_input_claims {
            proto_inputs.push(Proto::TransactionInput {
                txid: input.txid.to_vec().into(),
                vout: input.vout,
                sequence: input.sequence,
                script_sig: input.script_sig,
                witness_items: input.witness_items,
            });
        }

        // Prepare `Proto::TransactionOutput` protobufs for output.
        let mut proto_outputs = vec![];
        for output in utxo_outputs {
            proto_outputs.push(Proto::TransactionOutput {
                recipient: Cow::default(),
                script_pubkey: output.script_pubkey,
                amount: output.value,
                // TODO:
                control_block: None,
            });
        }

        // Prepare `Proto::Transaction` protobuf for output.
        let transaction = Proto::Transaction {
            version: utxo_preserializtion.version,
            // TODO
            lock_time: 0,
            inputs: proto_inputs,
            outputs: proto_outputs,
        };

        // Return the full protobuf output.
        Proto::SigningOutput {
            // TODO: This should be returned by tw_utxo.
            //transaction: Some(transaction),
            transaction: None,
            encoded: utxo_serialized.encoded,
            // TODO: Should be returned by `tw_utxo`.
            transaction_id: Cow::default(),
            error: 0,
            fee: 0,
        }
    }

    #[inline]
    fn json_signer(&self) -> Option<Self::JsonSigner> {
        None
    }

    #[inline]
    fn plan_builder(&self) -> Option<Self::PlanBuilder> {
        None
    }
}

fn pubkey_hash_from_proto(pubkey_or_hash: &Proto::ToPublicKeyOrHash) -> Result<PubkeyHash> {
    let pubkey_hash = match &pubkey_or_hash.to_address {
        ProtoPubkeyOrHash::hash(hash) => PubkeyHash::from_slice(hash.as_ref()).unwrap(),
        ProtoPubkeyOrHash::pubkey(pubkey) => bitcoin::PublicKey::from_slice(pubkey.as_ref())
            .unwrap()
            .pubkey_hash(),
        ProtoPubkeyOrHash::None => return Err(crate::Error::Todo),
    };

    Ok(pubkey_hash)
}

fn witness_pubkey_hash_from_proto(
    pubkey_or_hash: &Proto::ToPublicKeyOrHash,
) -> Result<WPubkeyHash> {
    let wpubkey_hash = match &pubkey_or_hash.to_address {
        ProtoPubkeyOrHash::hash(hash) => WPubkeyHash::from_slice(hash.as_ref()).unwrap(),
        ProtoPubkeyOrHash::pubkey(pubkey) => bitcoin::PublicKey::from_slice(pubkey.as_ref())
            .unwrap()
            .wpubkey_hash()
            .unwrap(),
        ProtoPubkeyOrHash::None => todo!(),
    };

    Ok(wpubkey_hash)
}

fn process_recipients<'a>(outputs: &Vec<Proto::Output<'a>>) -> Vec<UtxoProto::TxOut<'static>> {
    let mut utxo_outputs = vec![];

    let secp = secp256k1::Secp256k1::new();

    for output in outputs {
        let script_pubkey = match &output.to_recipient {
            // Script spending condition was passed on directly.
            ProtoOutputRecipient::script_pubkey(script) => ScriptBuf::from_bytes(script.to_vec()),
            // Process builder methods. We construct the Script spending
            // conditions by using the specified parameters.
            ProtoOutputRecipient::builder(builder) => match &builder.type_pb {
                ProtoBuilderType::p2sh(_) => {
                    todo!()
                },
                ProtoBuilderType::p2pkh(pubkey_or_hash) => {
                    let pubkey_hash = pubkey_hash_from_proto(pubkey_or_hash).unwrap();
                    ScriptBuf::new_p2pkh(&pubkey_hash)
                },
                ProtoBuilderType::p2wsh(_) => {
                    todo!()
                },
                ProtoBuilderType::p2wpkh(pubkey_or_hash) => {
                    let wpubkey_hash = witness_pubkey_hash_from_proto(pubkey_or_hash).unwrap();
                    ScriptBuf::new_v0_p2wpkh(&wpubkey_hash)
                },
                ProtoBuilderType::p2tr_key_path(pubkey) => {
                    let pubkey = bitcoin::PublicKey::from_slice(pubkey.as_ref()).unwrap();
                    let xonly = XOnlyPublicKey::from(pubkey.inner);
                    ScriptBuf::new_v1_p2tr(&secp, xonly, None)
                },
                ProtoBuilderType::p2tr_script_path(complex) => {
                    let node_hash = TapNodeHash::from_slice(complex.node_hash.as_ref()).unwrap();

                    let pubkey =
                        bitcoin::PublicKey::from_slice(complex.public_key.as_ref()).unwrap();
                    let xonly = XOnlyPublicKey::from(pubkey.inner);

                    ScriptBuf::new_v1_p2tr(&secp, xonly, Some(node_hash))
                },
                ProtoBuilderType::None => todo!(),
            },
            // We derive the spending condition for the address.
            ProtoOutputRecipient::from_address(_) => todo!(),
            ProtoOutputRecipient::None => todo!(),
        };

        utxo_outputs.push(UtxoProto::TxOut {
            value: output.amount,
            script_pubkey: script_pubkey.to_vec().into(),
        });
    }

    utxo_outputs
}
