pub mod create_client {
    use super::super::*;

    #[async_trait]
    pub trait CreateClientCheck: StateExt {
        async fn validate(&self, msg: &MsgCreateAnyClient) -> anyhow::Result<()> {
            let id_counter = self.client_counter().await?;
            ClientId::new(msg.client_state.client_type(), id_counter.0)?;

            Ok(())
        }
    }

    impl<T: StateExt> CreateClientCheck for T {}
}

pub mod update_client {
    use super::super::*;

    #[async_trait]
    pub trait UpdateClientCheck: StateExt + inner::Inner {
        async fn validate(&self, msg: &MsgUpdateAnyClient) -> anyhow::Result<()> {
            let client_data = self.client_is_present(msg).await?;

            client_is_not_frozen(&client_data)?;
            self.client_is_not_expired(&client_data).await?;

            let trusted_client_state =
                downcast!(client_data.client_state.0 => AnyClientState::Tendermint)
                    .ok_or_else(|| anyhow::anyhow!("invalid client state: not Tendermint"))?;

            let untrusted_header = downcast!(&msg.header => AnyHeader::Tendermint)
                .ok_or_else(|| anyhow::anyhow!("invalid header: not Tendermint"))?;

            // Optimization: reject duplicate updates instead of verifying them.
            if self
                .update_is_already_committed(&client_data.client_id, &untrusted_header)
                .await?
            {
                // If the update is already committed, return an error to reject a duplicate update.
                return Err(anyhow::anyhow!(
                    "Client update has already been committed to the chain state"
                ));
            }

            header_revision_matches_client_state(&trusted_client_state, &untrusted_header)?;
            header_height_is_consistent(&untrusted_header)?;

            // The (still untrusted) header uses the `trusted_height` field to
            // specify the trusted anchor data it is extending.
            let trusted_height = untrusted_header.trusted_height;

            // We use the specified trusted height to query the trusted
            // consensus state the update extends.
            let last_trusted_consensus_state = self
                .get_verified_consensus_state(trusted_height, client_data.client_id)
                .await?
                .as_tendermint()?;

            // We also have to convert from an IBC height, which has two
            // components, to a Tendermint height, which has only one.
            let trusted_height = trusted_height
                .revision_height
                .try_into()
                .map_err(|_| anyhow::anyhow!("invalid header height"))?;

            let trusted_validator_set =
                verify_header_validator_set(&untrusted_header, &last_trusted_consensus_state)?;

            // Now we build the trusted and untrusted states to feed to the Tendermint light client.

            let trusted_state = TrustedBlockState {
                header_time: last_trusted_consensus_state.timestamp,
                height: trusted_height,
                next_validators: &trusted_validator_set,
                next_validators_hash: last_trusted_consensus_state.next_validators_hash,
            };

            let untrusted_state = UntrustedBlockState {
                signed_header: &untrusted_header.signed_header,
                validators: &untrusted_header.validator_set,
                next_validators: None, // TODO: do we need this?
            };

            let options = trusted_client_state.as_light_client_options()?;
            let verifier = ProdVerifier::default();

            let verdict = verifier.verify(
                untrusted_state,
                trusted_state,
                &options,
                self.get_block_timestamp().await?,
            );

            match verdict {
                Verdict::Success => Ok(()),
                Verdict::NotEnoughTrust(voting_power_tally) => Err(anyhow::anyhow!(
                    "not enough trust, voting power tally: {:?}",
                    voting_power_tally
                )),
                Verdict::Invalid(detail) => Err(anyhow::anyhow!(
                    "could not verify tendermint header: invalid: {:?}",
                    detail
                )),
            }
        }
    }

    fn client_is_not_frozen(client: &ClientData) -> anyhow::Result<()> {
        if client.client_state.0.is_frozen() {
            Err(anyhow::anyhow!("client is frozen"))
        } else {
            Ok(())
        }
    }

    fn header_revision_matches_client_state(
        trusted_client_state: &TendermintClientState,
        untrusted_header: &TendermintHeader,
    ) -> anyhow::Result<()> {
        if untrusted_header.height().revision_number != trusted_client_state.chain_id.version() {
            Err(anyhow::anyhow!(
                "client update revision number does not match client state"
            ))
        } else {
            Ok(())
        }
    }

    fn header_height_is_consistent(untrusted_header: &TendermintHeader) -> anyhow::Result<()> {
        if untrusted_header.height() <= untrusted_header.trusted_height {
            Err(anyhow::anyhow!(
                "client update height is not greater than trusted height"
            ))
        } else {
            Ok(())
        }
    }

    fn verify_header_validator_set<'h>(
        untrusted_header: &'h TendermintHeader,
        last_trusted_consensus_state: &TendermintConsensusState,
    ) -> anyhow::Result<&'h validator::Set> {
        if untrusted_header.trusted_validator_set.hash()
            != last_trusted_consensus_state.next_validators_hash
        {
            Err(anyhow::anyhow!(
                "client update validator set hash does not match trusted consensus state"
            ))
        } else {
            Ok(&untrusted_header.trusted_validator_set)
        }
    }

    mod inner {
        use super::*;

        #[async_trait]
        pub trait Inner: StateExt {
            async fn client_is_present(
                &self,
                msg: &MsgUpdateAnyClient,
            ) -> anyhow::Result<ClientData> {
                self.get_client_data(&msg.client_id).await
            }

            async fn client_is_not_expired(&self, client: &ClientData) -> anyhow::Result<()> {
                let latest_consensus_state = self
                    .get_verified_consensus_state(
                        client.client_state.0.latest_height(),
                        client.client_id.clone(),
                    )
                    .await?;

                let latest_consensus_state_tm =
                    downcast!(latest_consensus_state.0 => AnyConsensusState::Tendermint)
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "invalid consensus state: not a Tendermint consensus state"
                            )
                        })?;

                let now = self.get_block_timestamp().await?;
                let time_elapsed = now.duration_since(latest_consensus_state_tm.timestamp)?;

                if client.client_state.0.expired(time_elapsed) {
                    Err(anyhow::anyhow!("client is expired"))
                } else {
                    Ok(())
                }
            }

            async fn update_is_already_committed(
                &self,
                client_id: &ClientId,
                untrusted_header: &TendermintHeader,
            ) -> anyhow::Result<bool> {
                // check if we already have a consensus state for this height, if we do, check that it is
                // the same as this update, if it is, return early.
                let untrusted_consensus_state =
                    TendermintConsensusState::from(untrusted_header.clone());
                if let Ok(stored_consensus_state) = self
                    .get_verified_consensus_state(untrusted_header.height(), client_id.clone())
                    .await
                {
                    let stored_tm_consensus_state = stored_consensus_state.as_tendermint()?;

                    Ok(stored_tm_consensus_state == untrusted_consensus_state)
                } else {
                    // If we don't have a consensus state for this height for
                    // whatever reason (either missing or a DB error), we don't
                    // consider it an error, it's just not already committed.
                    Ok(false)
                }
            }
        }

        impl<T: StateExt> Inner for T {}
    }

    impl<T: StateExt> UpdateClientCheck for T {}
}
