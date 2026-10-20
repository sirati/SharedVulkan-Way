//! Portable state/lifetime regression harness using the production runtime with
//! simulated native images and decoder events. This is not a hardware oracle.
#![allow(dead_code)]
mod native_video {
    use ash::vk;
    use std::sync::Arc;
    #[derive(Clone)]
    pub struct H264Reference {
        pub frame_num: u16,
        pub pic_order_cnt: [i32; 2],
        pub long_term: bool,
    }
    #[derive(Clone)]
    pub struct Picture {
        pub layer: u32,
        pub offset: vk::Offset2D,
        pub extent: vk::Extent2D,
    }
    #[derive(Clone)]
    pub struct Reference {
        pub slot: u32,
        pub picture: Picture,
        pub h264: H264Reference,
    }
    pub struct Session {
        pub id: u64,
    }
    pub struct H264Picture {
        pub idr_pic_flag: bool,
        pub is_intra: bool,
        pub is_reference: bool,
        pub frame_num: u16,
        pub pic_order_cnt: [i32; 2],
    }
    pub struct Configuration {
        pub sps: Vec<u8>,
        pub pps: Vec<u8>,
    }
    pub struct Decode {
        pub session: Arc<Session>,
        pub references: Vec<Reference>,
        pub begin_references: Vec<Reference>,
        pub setup: Option<Reference>,
        pub source_range: u64,
        pub slice_offsets: Vec<u32>,
        pub destination: Picture,
        pub h264: H264Picture,
        pub reset_before: bool,
        pub begin_before: bool,
        pub configuration: Configuration,
    }
}
mod native_video_image {
    use ash::vk;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    #[derive(Default)]
    pub struct Image {
        pub writes: AtomicUsize,
        pub fail: bool,
    }
    impl Image {
        pub fn validate_bound(&self) -> Result<(), vk::Result> {
            Ok(())
        }
        pub fn extent(&self) -> vk::Extent2D {
            vk::Extent2D {
                width: 64,
                height: 64,
            }
        }
        pub unsafe fn write_completed(&self, _: Arc<()>) -> Result<(), vk::Result> {
            if self.fail {
                return Err(vk::Result::ERROR_DEVICE_LOST);
            }
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
}
mod video_nv12 {
    pub mod videotoolbox {
        use std::{
            ops::Range,
            sync::{
                Arc,
                mpsc::{self, Receiver},
            },
        };
        pub unsafe trait CompressedBytes: Send + Sync {
            fn bytes(&self) -> &[u8];
        }
        pub struct Decoder {
            pub coded_extent: [u32; 2],
        }
        pub struct Submission {
            pub picture: Receiver<Result<Arc<()>, String>>,
            pub input_released: Receiver<()>,
        }
        impl Decoder {
            pub fn new_coded(_: &[u8], _: &[u8]) -> Result<Self, String> {
                Ok(Self {
                    coded_extent: [64, 64],
                })
            }
            pub fn update_parameters(&mut self, _: &[u8], _: &[u8]) -> Result<(), String> {
                Ok(())
            }
            pub fn drain(&mut self) -> Result<(), String> {
                Ok(())
            }
            pub fn submit_annex_b_tracked(
                &mut self,
                source: Arc<dyn CompressedBytes>,
                _: Range<usize>,
                _: &[u32],
            ) -> Result<Submission, String> {
                let (p, pr) = mpsc::channel();
                let (i, ir) = mpsc::channel();
                drop(source);
                i.send(()).unwrap();
                p.send(Ok(Arc::new(()))).unwrap();
                Ok(Submission {
                    picture: pr,
                    input_released: ir,
                })
            }
        }
    }
}
#[path = "../src/native_video_decode.rs"]
mod runtime;
#[cfg(test)]
mod integration {
    use super::*;
    use native_video::*;
    use native_video_image::Image;
    use runtime::*;
    use std::sync::{Arc, atomic::Ordering};
    struct Bytes(Vec<u8>);
    unsafe impl video_nv12::videotoolbox::CompressedBytes for Bytes {
        fn bytes(&self) -> &[u8] {
            &self.0
        }
    }
    fn picture() -> Picture {
        Picture {
            layer: 0,
            offset: ash::vk::Offset2D { x: 0, y: 0 },
            extent: ash::vk::Extent2D {
                width: 64,
                height: 64,
            },
        }
    }
    fn job() -> Decode {
        Decode {
            session: Arc::new(Session { id: 1 }),
            references: vec![],
            begin_references: vec![],
            setup: Some(Reference {
                slot: 0,
                picture: picture(),
                h264: H264Reference {
                    frame_num: 0,
                    pic_order_cnt: [0, 0],
                    long_term: false,
                },
            }),
            source_range: 5,
            slice_offsets: vec![0],
            destination: picture(),
            h264: H264Picture {
                idr_pic_flag: false,
                is_intra: false,
                is_reference: true,
                frame_num: 0,
                pic_order_cnt: [0, 0],
            },
            reset_before: false,
            begin_before: false,
            configuration: Configuration {
                sps: vec![],
                pps: vec![],
            },
        }
    }
    fn resources(dst: Arc<Image>, setup: Option<Arc<Image>>, idr: bool) -> Resources {
        Resources {
            source: Arc::new(Bytes(vec![
                0,
                0,
                1,
                if idr { 0x65 } else { 0x41 },
                if idr { 0xb8 } else { 0xe0 },
            ])),
            source_range: 0..5,
            destination: dst,
            setup,
            references: vec![],
            begin_references: vec![],
        }
    }
    #[test]
    fn stock_idr_bootstrap_and_exact_distinct_setup_writes() {
        let mut rt = Runtime::default();
        let j = job();
        let dst = Arc::new(Image::default());
        let dpb = Arc::new(Image::default());
        unsafe { rt.execute(&j, resources(dst.clone(), Some(dpb.clone()), true)) }.unwrap();
        assert_eq!(dst.writes.load(Ordering::SeqCst), 1);
        assert_eq!(dpb.writes.load(Ordering::SeqCst), 1);
        let mut dependent = job();
        dependent.session = j.session;
        dependent.h264.frame_num = 1;
        dependent.h264.pic_order_cnt = [2, 2];
        dependent.setup = None;
        dependent.references = vec![j.setup.unwrap()];
        let mut r = resources(dst.clone(), None, false);
        r.references.push(dpb.clone());
        unsafe { rt.execute(&dependent, r) }.unwrap();
        let mut wrong = resources(dst, None, false);
        wrong.references.push(Arc::new(Image::default()));
        assert!(unsafe { rt.execute(&dependent, wrong) }.is_err());
    }
    #[test]
    fn accepted_copy_failure_poison_is_sticky() {
        let mut rt = Runtime::default();
        let j = job();
        let dst = Arc::new(Image {
            fail: true,
            ..Default::default()
        });
        assert!(
            unsafe { rt.execute(&j, resources(dst, Some(Arc::new(Image::default())), true)) }
                .is_err()
        );
        assert!(
            unsafe {
                rt.execute(
                    &j,
                    resources(
                        Arc::new(Image::default()),
                        Some(Arc::new(Image::default())),
                        true,
                    ),
                )
            }
            .unwrap_err()
            .contains("poisoned")
        );
    }
    #[test]
    fn reset_refuses_non_idr_bootstrap() {
        let mut rt = Runtime::default();
        let j = job();
        assert!(
            unsafe {
                rt.execute(
                    &j,
                    resources(
                        Arc::new(Image::default()),
                        Some(Arc::new(Image::default())),
                        false,
                    ),
                )
            }
            .unwrap_err()
            .contains("independent intra")
        );
    }
    #[test]
    fn nonreference_setup_invalidates_slot_and_same_target_copies_once() {
        let mut rt = Runtime::default();
        let mut j = job();
        let target = Arc::new(Image::default());
        unsafe { rt.execute(&j, resources(target.clone(), Some(target.clone()), true)) }.unwrap();
        assert_eq!(target.writes.load(Ordering::SeqCst), 1);
        let previous = j.setup.clone().unwrap();
        j.h264.is_reference = false;
        j.h264.is_intra = true;
        let mut nonreference = resources(target.clone(), Some(target.clone()), false);
        nonreference.source = Arc::new(Bytes(vec![0, 0, 1, 0x01, 0xb8]));
        unsafe { rt.execute(&j, nonreference) }.unwrap();
        j.setup = None;
        j.references.push(previous);
        let mut r = resources(target.clone(), None, false);
        r.references.push(target);
        r.source = Arc::new(Bytes(vec![0, 0, 1, 0x01, 0xb8]));
        assert!(
            unsafe { rt.execute(&j, r) }
                .unwrap_err()
                .contains("unknown native DPB slot")
        );
    }
    #[test]
    fn original_non_idr_i_bootstraps_and_resets_despite_absent_stock_intra_flag() {
        let bytes = include_bytes!("../examples/assets/videotoolbox-h264/non-idr-intra-64x50.h264");
        let mut j = job();
        j.h264.frame_num = 3;
        j.h264.pic_order_cnt = [8, 8];
        j.source_range = 121;
        j.configuration = Configuration {
            sps: bytes[4..26].to_vec(),
            pps: bytes[30..35].to_vec(),
        };
        let setup = j.setup.as_mut().unwrap();
        setup.h264.frame_num = 3;
        setup.h264.pic_order_cnt = [8, 8];
        let dst = Arc::new(Image::default());
        let dpb = Arc::new(Image::default());
        let mut runtime = Runtime::default();
        for reset in [false, true] {
            j.reset_before = reset;
            let mut r = resources(dst.clone(), Some(dpb.clone()), false);
            r.source = Arc::new(Bytes(bytes.to_vec()));
            r.source_range = 35..156;
            unsafe { runtime.execute(&j, r) }.unwrap();
        }
        assert_eq!(dst.writes.load(Ordering::SeqCst), 2);
        assert_eq!(dpb.writes.load(Ordering::SeqCst), 2);
    }
    #[test]
    fn unproven_nonreference_intra_bootstrap_remains_gated() {
        let mut j = job();
        j.h264.is_reference = false;
        let mut r = resources(
            Arc::new(Image::default()),
            Some(Arc::new(Image::default())),
            false,
        );
        r.source = Arc::new(Bytes(vec![0, 0, 1, 0x01, 0xb8]));
        assert!(
            unsafe { Runtime::default().execute(&j, r) }
                .unwrap_err()
                .contains("independent intra reference")
        );
    }
}
