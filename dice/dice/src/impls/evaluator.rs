/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::sync::Arc;

use dupe::Dupe;

use crate::api::error::DiceResult;
use crate::ctx::DiceComputationsImpl;
use crate::impls::ctx::PerComputeCtx;
use crate::impls::ctx::SharedLiveTransactionCtx;
use crate::impls::dice::DiceModern;
use crate::impls::key::DiceKey;
use crate::impls::key::DiceKeyErasedRef;
use crate::impls::transaction::ActiveTransactionGuard;
use crate::impls::value::DiceValue;
use crate::DiceComputations;
use crate::DiceProjectionComputations;
use crate::HashSet;
use crate::UserComputationData;

/// Evaluates Keys
#[allow(unused)]
#[derive(Clone, Dupe)]
pub(crate) struct AsyncEvaluator {
    per_live_version_ctx: Arc<SharedLiveTransactionCtx>,
    live_version_guard: ActiveTransactionGuard,
    user_data: Arc<UserComputationData>,
    dice: Arc<DiceModern>,
}

#[allow(unused)]
impl AsyncEvaluator {
    pub(crate) fn new(
        per_live_version_ctx: Arc<SharedLiveTransactionCtx>,
        live_version_guard: ActiveTransactionGuard,
        user_data: Arc<UserComputationData>,
        dice: Arc<DiceModern>,
    ) -> Self {
        Self {
            per_live_version_ctx,
            live_version_guard,
            user_data,
            dice,
        }
    }

    pub(crate) async fn evaluate<'b>(
        &self,
        key: DiceKeyErasedRef<'b>,
    ) -> DiceResult<DiceValueAndDeps> {
        match key {
            DiceKeyErasedRef::Key(key) => {
                let new_ctx = DiceComputations(DiceComputationsImpl::Modern(PerComputeCtx::new(
                    self.per_live_version_ctx.dupe(),
                    self.live_version_guard.dupe(),
                    self.user_data.dupe(),
                    self.dice.dupe(),
                )));

                let value = key.compute(&new_ctx).await;
                let deps = match new_ctx.0 {
                    DiceComputationsImpl::Legacy(_) => {
                        unreachable!("modern dice created above")
                    }
                    DiceComputationsImpl::Modern(new_ctx) => new_ctx.finalize_deps(),
                };

                Ok(DiceValueAndDeps { value, deps })
            }
            DiceKeyErasedRef::Projection(proj) => {
                let base = self
                    .per_live_version_ctx
                    .compute_opaque(proj.base(), self.dupe())
                    .await?;

                let ctx = DiceProjectionComputations {
                    data: &self.dice.global_data,
                    user_data: &self.user_data,
                };

                let value = proj.proj().compute(base, &ctx);

                Ok(DiceValueAndDeps {
                    value,
                    deps: [proj.base()].into_iter().collect(),
                })
            }
        }
    }
}

#[allow(unused)] // TODO(bobyf)
pub(crate) struct DiceValueAndDeps {
    pub(crate) value: DiceValue,
    pub(crate) deps: HashSet<DiceKey>,
}