/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! <https://w3c.github.io/pointerlock/>
//!
//! 最小の Pointer Lock 実装。engine は DOM の状態遷移と promise 解決だけを持ち、
//! OS の cursor grab は embedder（xrift-client）が所有する。
//! JS shim や console magic prefix は使わない。
//!
//! 要求にはプロセス内で一意な世代を付け、embedder まで往復させる。
//! 同じ要素の「要求→キャンセル→再要求」で古い応答が新しい要求に適用されたり、
//! stale な解放通知が別世代の grab を解いたりする取り違えを防ぐ。

use std::cell::RefCell;
use std::mem;
use std::sync::atomic::{AtomicU64, Ordering};

use embedder_traits::{EmbedderMsg, PointerLockResponse};
use js::context::JSContext;
use js::realm::CurrentRealm;
use stylo_atoms::Atom;

use crate::dom::RootedPromise;
use crate::dom::bindings::codegen::Bindings::ElementBinding::PointerLockOptions;
use crate::dom::bindings::error::Error;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::TrustedPromise;
use crate::dom::bindings::root::DomRoot;
use crate::dom::document::Document;
use crate::dom::element::Element;
use crate::dom::eventtarget::EventTarget;
use crate::dom::node::Node;
use crate::dom::promise::Promise;
use crate::routed_promise::{RoutedPromiseListener, callback_promise};

/// 要求世代の払い出し。0 は使わず 1 から始める。
static NEXT_POINTER_LOCK_GENERATION: AtomicU64 = AtomicU64::new(1);

/// 同一要素への同時 `requestPointerLock` を合流させる待機 promise 群。
/// 中身の `TrustedPromise` は promise 保持表で GC 保護するため trace 対象外とする。
/// 借用中に script を実行しないため通常の `RefCell` で足りる。
/// `Document` のフィールドとして保持し、`#[no_trace]` で trace を外す。
#[derive(Default)]
pub(crate) struct PointerLockWaiters(RefCell<Vec<TrustedPromise>>);

impl PointerLockWaiters {
    fn push(&self, promise: TrustedPromise) {
        self.0.borrow_mut().push(promise);
    }

    fn take(&self) -> Vec<TrustedPromise> {
        mem::take(&mut *self.0.borrow_mut())
    }

    fn clear(&self) {
        self.0.borrow_mut().clear();
    }
}

impl Document {
    /// ロック中の要素。`None` は未ロック。
    pub(crate) fn pointer_lock_element(&self) -> Option<DomRoot<Element>> {
        self.pointer_lock_element_holder().get()
    }

    pub(crate) fn set_pointer_lock_element(&self, element: Option<&Element>) {
        self.pointer_lock_element_holder().set(element);
    }

    /// ロック中か。
    pub(crate) fn is_pointer_locked(&self) -> bool {
        self.pointer_lock_element_holder().get().is_some()
    }

    pub(crate) fn pointer_lock_pending(&self) -> bool {
        self.pointer_lock_pending_cell().get()
    }

    pub(crate) fn set_pointer_lock_pending(&self, pending: bool) {
        self.pointer_lock_pending_cell().set(pending);
    }

    /// <https://w3c.github.io/pointerlock/#dom-element-requestpointerlock>
    /// の engine 側。本体は embedder の応答待ちで promise を返す。
    pub(crate) fn request_pointer_lock(
        &self,
        cx: &mut CurrentRealm,
        element: &Element,
        options: &PointerLockOptions,
    ) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);

        // 未対応のオプションは成功に見せない。
        if options.unadjustedMovement {
            promise.reject_error(
                cx,
                Error::NotSupported(Some("unadjustedMovement is not supported".to_owned())),
            );
            return promise;
        }

        // fully active / 接続済みを確認する。だめなら reject。
        if !self.is_fully_active() || !element.upcast::<Node>().is_connected() {
            promise.reject_error(
                cx,
                Error::InvalidState(Some(
                    "Document is not fully active or element is not connected".to_owned(),
                )),
            );
            return promise;
        }
        if self.pointer_lock_pending() {
            // 同一要素への同時要求は合流させる。新しい世代は払い出さず、
            // 追加の embedder 要求は送らない。応答時に一次 promise と同じ
            // 結果で解決する。activation も消費しない。
            // XRift は 1 click で複数コンポーネントから同じ要素へ要求するため、
            // pending 中の reject が console error として連発するのを避ける。
            if self
                .pointer_lock_pending_element_holder()
                .get()
                .as_deref() ==
                Some(element)
            {
                self.pointer_lock_waiters()
                    .push(TrustedPromise::new(&promise));
                return promise;
            }
            promise.reject_error(
                cx,
                Error::InvalidState(Some("Pointer lock request is already pending".to_owned())),
            );
            return promise;
        }

        // 既に同じ要素がロック中なら即 resolve。
        if self.pointer_lock_element().as_deref() == Some(element) {
            promise.resolve_native(cx, &());
            return promise;
        }

        // transient activation を確認し、あれば消費する。無ければ既存ロックを
        // 壊さず reject する（解除より前）。
        let window = self.window();
        if !window.has_transient_activation() {
            promise.reject_error(
                cx,
                Error::NotAllowed(Some(
                    "Requesting pointer lock requires transient activation".to_owned(),
                )),
            );
            return promise;
        }
        window.consume_user_activation();

        // 別要素がロック中なら先に解除する。embedder へも通知する。
        // CurrentRealm は JSContext へ Deref できるため、そのまま渡す。
        if self.is_pointer_locked() {
            self.exit_pointer_lock(cx, true);
        }

        // 新規要求に世代を払い出し、embedder まで往復させる。
        // 応答の receiver は Document 自身にする。要求後に element が別
        // Document へ adopt されても、要求元の Document に応答が返る。
        let generation = NEXT_POINTER_LOCK_GENERATION.fetch_add(1, Ordering::Relaxed);
        self.set_pointer_lock_pending(true);
        self.pointer_lock_pending_generation_cell()
            .set(Some(generation));
        self.pointer_lock_pending_element_holder()
            .set(Some(element));
        self.pointer_lock_waiters().clear();
        self.pointer_lock_primary()
            .borrow_mut()
            .replace(TrustedPromise::new(&promise));
        let task_manager = self.task_manager();
        let task_source = task_manager.dom_manipulation_task_source();
        let callback = callback_promise(&promise, self, task_source);
        self.send_to_embedder(EmbedderMsg::RequestPointerLock(
            self.webview_id(),
            generation,
            callback,
        ));

        promise
    }

    /// embedder からの応答を処理する。世代で要求と対応付ける。
    pub(crate) fn handle_pointer_lock_response(
        &self,
        cx: &mut JSContext,
        response: PointerLockResponse,
        promise: &RootedPromise,
    ) {
        // 世代が一致しない応答は stale。pending 中の別要求の状態には触らない。
        // Allow でも再ロックしない。Deny は解放不要なので reject のみ。
        if self.pointer_lock_pending_generation_cell().get() != Some(response.generation) {
            if response.allowed {
                // grab が残らないよう embedder に解放させる。世代付きのため
                // 別世代の grab を解くことはない。
                self.send_to_embedder(EmbedderMsg::NotifyPointerLockStateChanged(
                    self.webview_id(),
                    false,
                    response.generation,
                ));
            }
            promise.reject_error(
                cx,
                Error::InvalidState(Some("Pointer lock response is stale".to_owned())),
            );
            return;
        }
        self.set_pointer_lock_pending(false);
        self.pointer_lock_pending_generation_cell().set(None);
        let pending_element = self.pointer_lock_pending_element_holder().get();
        self.pointer_lock_pending_element_holder().set(None);
        // 一次 promise の pin を取り出す。callback 経由の promise と同一のため、
        // こちらで settle し、pin の後始末を兼ねる。
        let primary_rooted: Option<RootedPromise> = self
            .pointer_lock_primary()
            .borrow_mut()
            .take()
            .map(|pin| pin.root(cx));
        let primary: &RootedPromise = primary_rooted.as_ref().unwrap_or(promise);
        let waiters = self.pointer_lock_waiters().take();

        if !response.allowed {
            self.upcast::<EventTarget>()
                .fire_event(cx, Atom::from("pointerlockerror"));
            primary.reject_error(
                cx,
                Error::NotSupported(Some("Pointer lock was denied".to_owned())),
            );
            for waiter in waiters {
                waiter.root(cx).reject_error(
                    cx,
                    Error::NotSupported(Some("Pointer lock was denied".to_owned())),
                );
            }
            return;
        }

        // fully active / 接続済みを再確認する。だめなら解放して reject。
        let connected = pending_element
            .as_deref()
            .is_some_and(|element| element.upcast::<Node>().is_connected());
        if !self.is_fully_active() || !connected {
            self.send_to_embedder(EmbedderMsg::NotifyPointerLockStateChanged(
                self.webview_id(),
                false,
                response.generation,
            ));
            self.upcast::<EventTarget>()
                .fire_event(cx, Atom::from("pointerlockerror"));
            primary.reject_error(
                cx,
                Error::InvalidState(Some(
                    "Document is not fully active or element is not connected".to_owned(),
                )),
            );
            for waiter in waiters {
                waiter.root(cx).reject_error(
                    cx,
                    Error::InvalidState(Some(
                        "Document is not fully active or element is not connected".to_owned(),
                    )),
                );
            }
            return;
        }
        self.set_pointer_lock_element(pending_element.as_deref());
        self.pointer_lock_locked_generation_cell()
            .set(Some(response.generation));
        self.upcast::<EventTarget>()
            .fire_event(cx, Atom::from("pointerlockchange"));
        primary.resolve_native(cx, &());
        for waiter in waiters {
            waiter.root(cx).resolve_native(cx, &());
        }
    }

    /// script 起点の解除。`notify_embedder` が真なら embedder へ通知する。
    pub(crate) fn exit_pointer_lock(&self, cx: &mut JSContext, notify_embedder: bool) {
        // 保留中の要求を取り消す。一次 promise もここで settle する。
        // 遅延応答は世代不一致で stale 扱いになる。
        if self.pointer_lock_pending() {
            self.set_pointer_lock_pending(false);
            self.pointer_lock_pending_generation_cell().set(None);
            self.pointer_lock_pending_element_holder().set(None);
            let waiters = self.pointer_lock_waiters().take();
            if let Some(pin) = self.pointer_lock_primary().borrow_mut().take() {
                pin.root(cx).reject_error(
                    cx,
                    Error::InvalidState(Some("Pointer lock request was cancelled".to_owned())),
                );
            }
            for waiter in waiters {
                waiter.root(cx).reject_error(
                    cx,
                    Error::InvalidState(Some("Pointer lock request was cancelled".to_owned())),
                );
            }
        }
        if !self.is_pointer_locked() {
            return;
        }
        let generation = self.pointer_lock_locked_generation_cell().take();
        self.set_pointer_lock_element(None);
        self.upcast::<EventTarget>()
            .fire_event(cx, Atom::from("pointerlockchange"));
        if notify_embedder {
            self.send_to_embedder(EmbedderMsg::NotifyPointerLockStateChanged(
                self.webview_id(),
                false,
                // locked 中は世代が必ずある。無い場合は 0（無効な世代）。
                generation.unwrap_or(0),
            ));
        }
    }

    /// native 起点の解除。embedder へは返さない（無限往復を避ける）。
    pub(crate) fn exit_pointer_lock_from_embedder(&self, cx: &mut JSContext) {
        self.exit_pointer_lock(cx, false);
    }
}

impl RoutedPromiseListener<crate::DomTypeHolder, PointerLockResponse> for Document {
    fn handle_response(
        &self,
        cx: &mut JSContext,
        response: PointerLockResponse,
        promise: &RootedPromise,
    ) {
        self.handle_pointer_lock_response(cx, response, promise);
    }
}
