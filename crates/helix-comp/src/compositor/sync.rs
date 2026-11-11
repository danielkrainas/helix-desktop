//! Buffer synchronization (helix-desktop-spec `contracts/wayland/README.md`,
//! "Explicit sync"): commits of GPU buffers wait for their acquire point
//! (`wp_linux_drm_syncobj_v1`) or implicit dmabuf fences.

use smithay::backend::drm::DrmDeviceFd;
use smithay::reexports::calloop::Interest;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::wayland::compositor::{add_blocker, add_pre_commit_hook, with_states, BufferAssignment, CompositorHandler, SurfaceAttributes};
use smithay::wayland::dmabuf::get_dmabuf;
use smithay::wayland::drm_syncobj::{supports_syncobj_eventfd, DrmSyncobjCachedState, DrmSyncobjHandler, DrmSyncobjState};

use super::Helix;

impl Helix {
    /// Offer explicit sync when `device` can wait on syncobj points.
    pub fn enable_syncobj(&mut self, device: DrmDeviceFd) {
        if self.syncobj_state.is_some() {
            return;
        }
        if supports_syncobj_eventfd(&device) {
            self.syncobj_state = Some(DrmSyncobjState::new::<Helix>(&self.display_handle, device));
            tracing::info!("explicit sync offered");
        } else {
            tracing::info!("explicit sync not offered (no syncobj eventfd support)");
        }
    }

    /// Hold commits of GPU buffers until they are ready.
    pub fn add_commit_blockers(&mut self, surface: &WlSurface) {
        add_pre_commit_hook::<Self, _>(surface, |state, _dh, surface| {
            let mut acquire = None;
            let dmabuf = with_states(surface, |data| {
                acquire.clone_from(&data.cached_state.get::<DrmSyncobjCachedState>().pending().acquire_point);
                data.cached_state.get::<SurfaceAttributes>().pending().buffer.as_ref().and_then(|b| match b {
                    BufferAssignment::NewBuffer(buffer) => get_dmabuf(buffer).cloned().ok(),
                    _ => None,
                })
            });
            let Some(dmabuf) = dmabuf else { return };
            let Some(client) = surface.client() else { return };
            if let Some(acquire) = acquire {
                if let Ok((blocker, source)) = acquire.generate_blocker() {
                    let c = client.clone();
                    let inserted = state.loop_handle.insert_source(source, move |_, _, helix| {
                        let dh = helix.display_handle.clone();
                        helix.client_compositor_state(&c).blocker_cleared(helix, &dh);
                        Ok(())
                    });
                    if inserted.is_ok() {
                        add_blocker(surface, blocker);
                        return;
                    }
                }
            }
            if let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ) {
                let inserted = state.loop_handle.insert_source(source, move |_, _, helix| {
                    let dh = helix.display_handle.clone();
                    helix.client_compositor_state(&client).blocker_cleared(helix, &dh);
                    Ok(())
                });
                if inserted.is_ok() {
                    add_blocker(surface, blocker);
                }
            }
        });
    }
}

impl DrmSyncobjHandler for Helix {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.syncobj_state.as_mut()
    }
}

smithay::delegate_drm_syncobj!(Helix);
