use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::ImportDma;
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};

use crate::RubixState;

impl DmabufHandler for RubixState {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(&mut self, _global: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        // Import into the primary-GPU renderer to validate the buffer is usable.
        // `udev_handle` is Some on the udev backend; winit never creates the
        // dmabuf global, so this path is unreachable there.
        let ok = self
            .udev_handle
            .as_ref()
            .and_then(|udev| {
                let mut udev = udev.borrow_mut();
                let node = udev.primary_gpu;
                udev.gpus
                    .single_renderer(&node)
                    .ok()
                    .map(|mut r| r.import_dmabuf(&dmabuf, None).is_ok())
            })
            .unwrap_or(false);

        if ok {
            let _ = notifier.successful::<RubixState>();
        } else {
            notifier.failed();
        }
    }
}

// See handlers/mod.rs for the single `delegate_dispatch2!(RubixState)` call
// that now covers this (and every other) protocol.
