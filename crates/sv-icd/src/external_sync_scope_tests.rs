//! Supported external-sync scope and failed-import descriptor ownership.
use super::*;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;

#[test]
fn external_sync_scope_refuses_real_fd_without_consuming_or_closing_it() {
    crate::dmabuf::test_enabled_scope(|| {
        let icd = SharedVulkan::new(1 << 20).unwrap();
        // A real unreadable descriptor exercises ownership on refusal. It is
        // deliberately not claimed to be a kernel sync_file fixture.
        let (mut read, mut write) = std::os::unix::net::UnixStream::pair().unwrap();
        read.set_nonblocking(true).unwrap();
        let mut byte = [0];
        assert_eq!(
            read.read(&mut byte).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let mut info = p::VkImportSemaphoreFdInfoKHR {
            semaphore: p::VkSemaphore(20),
            flags: p::VkSemaphoreImportFlags(1),
            handle_type: p::VkExternalSemaphoreHandleTypeFlagBits(crate::dmabuf::SYNC_FD as i32),
            fd: read.as_raw_fd(),
            ..Default::default()
        };
        let reply = icd
            .import_semaphore_fd_khr(p::VkImportSemaphoreFdKHRArgs {
                device: p::VkDevice(1),
                p_import_semaphore_fd_info: Some(Box::new(info.clone())),
            })
            .unwrap();
        assert_eq!(reply.result.0, VK_ERROR_INVALID_EXTERNAL_HANDLE);
        assert!(!icd.state().semaphores.contains_key(&20));
        write.write_all(&[42]).unwrap();
        assert_eq!(read.read(&mut byte).unwrap(), 1);
        assert_eq!(
            byte[0], 42,
            "failed import must leave fd open and owned by the caller"
        );
        info.fd = -1;
        assert_eq!(
            icd.import_semaphore_fd_khr(p::VkImportSemaphoreFdKHRArgs {
                device: p::VkDevice(1),
                p_import_semaphore_fd_info: Some(Box::new(info))
            })
            .unwrap()
            .result
            .0,
            0
        );
        assert!(icd.state().semaphores[&20].temporary_presignalled);
    });
}
#[test]
fn external_sync_scope_advertises_export_without_pending_fd_import() {
    crate::dmabuf::test_enabled_scope(|| {
        let icd = SharedVulkan::new(1 << 20).unwrap();
        let properties = icd
            .get_physical_device_external_semaphore_properties(
                p::VkGetPhysicalDeviceExternalSemaphorePropertiesArgs {
                    physical_device: p::VkPhysicalDevice(1),
                    p_external_semaphore_info: Some(Box::new(
                        p::VkPhysicalDeviceExternalSemaphoreInfo {
                            handle_type: p::VkExternalSemaphoreHandleTypeFlagBits(
                                crate::dmabuf::SYNC_FD as i32,
                            ),
                            ..Default::default()
                        },
                    )),
                    ..Default::default()
                },
            )
            .unwrap()
            .p_external_semaphore_properties
            .unwrap();
        assert_eq!(properties.external_semaphore_features.0, 1);
        assert_eq!(properties.export_from_imported_handle_types.0, 0);
        assert_eq!(properties.compatible_handle_types.0, crate::dmabuf::SYNC_FD);
    });
}
