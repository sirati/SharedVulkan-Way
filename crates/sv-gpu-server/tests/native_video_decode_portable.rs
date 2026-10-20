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
        pub references: u32,
        pub dpb_slots: u32,
    }
    pub struct H264Picture {
        pub idr_pic_flag: bool,
        pub is_intra: bool,
        pub is_reference: bool,
        pub frame_num: u16,
        pub pic_order_cnt: [i32; 2],
        pub sps_id: u8,
        pub pps_id: u8,
        pub idr_pic_id: u16,
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
    #[derive(Default)]
    struct Writer(Vec<bool>);
    impl Writer {
        fn bit(&mut self, b: bool) {
            self.0.push(b)
        }
        fn fixed(&mut self, n: u32, count: u32) {
            for i in (0..count).rev() {
                self.bit(n >> i & 1 != 0)
            }
        }
        fn ue(&mut self, n: u32) {
            let v = n + 1;
            let width = 32 - v.leading_zeros();
            for _ in 1..width {
                self.bit(false)
            }
            self.fixed(v, width)
        }
        fn se(&mut self, n: i32) {
            self.ue(if n <= 0 {
                (-n as u32) * 2
            } else {
                n as u32 * 2 - 1
            })
        }
        fn nal(mut self, header: u8) -> Vec<u8> {
            self.bit(true);
            while self.0.len() % 8 != 0 {
                self.bit(false)
            }
            let mut out = vec![header];
            let mut zeros = 0;
            for chunk in self.0.chunks(8) {
                let n = chunk.iter().fold(0u8, |v, b| (v << 1) | u8::from(*b));
                if zeros == 2 && n <= 3 {
                    out.push(3);
                    zeros = 0
                }
                out.push(n);
                zeros = if n == 0 { zeros + 1 } else { 0 };
            }
            out
        }
    }
    fn parameters(weighted: bool, bipred: u32) -> (Vec<u8>, Vec<u8>) {
        let mut s = Writer::default();
        s.fixed(66, 8);
        s.fixed(0, 8);
        s.fixed(30, 8);
        s.ue(0);
        s.ue(0);
        s.ue(2);
        s.ue(4);
        s.bit(false);
        s.ue(3);
        s.ue(3);
        s.bit(true);
        s.bit(true);
        s.bit(false);
        s.bit(false);
        let mut p = Writer::default();
        p.ue(0);
        p.ue(0);
        p.bit(false);
        p.bit(false);
        p.ue(0);
        p.ue(0);
        p.ue(0);
        p.bit(weighted);
        p.fixed(bipred, 2);
        p.se(0);
        p.se(0);
        p.se(0);
        p.bit(true);
        p.bit(false);
        p.bit(false);
        (s.nal(0x67), p.nal(0x68))
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
    fn job(session: Arc<Session>, frame: u16) -> Decode {
        let (sps, pps) = parameters(false, 0);
        Decode {
            session,
            references: vec![],
            begin_references: vec![],
            setup: Some(reference(frame)),
            source_range: 0,
            slice_offsets: vec![0],
            destination: picture(),
            h264: H264Picture {
                idr_pic_flag: false,
                is_intra: false,
                is_reference: true,
                frame_num: frame,
                pic_order_cnt: [i32::from(frame) * 2; 2],
                sps_id: 0,
                pps_id: 0,
                idr_pic_id: 0,
            },
            reset_before: false,
            begin_before: false,
            configuration: Configuration { sps, pps },
        }
    }
    fn reference(frame: u16) -> Reference {
        Reference {
            slot: 0,
            picture: picture(),
            h264: H264Reference {
                frame_num: frame,
                pic_order_cnt: [i32::from(frame) * 2; 2],
                long_term: false,
            },
        }
    }
    fn session() -> Arc<Session> {
        Arc::new(Session {
            id: 1,
            references: 4,
            dpb_slots: 4,
        })
    }
    fn image() -> Arc<Image> {
        Arc::new(Image::default())
    }
    // Header-only NALs are used solely against the mock decoder. Real original
    // stream bootstrap below exercises the same production parser separately.
    fn resources(
        j: &mut Decode,
        dst: Arc<Image>,
        setup: Option<Arc<Image>>,
        idr: bool,
        intra: bool,
        operations: &[(u32, u32, u32)],
    ) -> Resources {
        let mut b = Writer::default();
        b.ue(0);
        b.ue(if intra { 2 } else { 0 });
        b.ue(0);
        b.fixed(j.h264.frame_num.into(), 4);
        if idr {
            b.ue(0)
        }
        if !intra {
            b.bit(false);
            b.bit(false);
        }
        if j.h264.is_reference {
            if idr {
                b.bit(false);
                b.bit(false)
            } else {
                b.bit(!operations.is_empty());
                if !operations.is_empty() {
                    for &(op, a, c) in operations {
                        b.ue(op);
                        if matches!(op, 1 | 2 | 3 | 4 | 6) {
                            b.ue(a)
                        }
                        if op == 3 {
                            b.ue(c)
                        }
                    }
                    b.ue(0)
                }
            }
        }
        let mut bytes = vec![0, 0, 1];
        bytes.extend(b.nal(if idr {
            0x65
        } else if j.h264.is_reference {
            0x41
        } else {
            0x01
        }));
        j.source_range = bytes.len() as u64;
        Resources {
            source_range: 0..bytes.len(),
            source: Arc::new(Bytes(bytes)),
            destination: dst,
            setup,
            references: vec![],
            begin_references: vec![],
        }
    }
    #[test]
    fn canonical_setup_only_and_missing_actual_list_are_enforced() {
        let mut rt = Runtime::default();
        let owner = session();
        let mut j = job(owner.clone(), 0);
        let dst = image();
        let dpb = image();
        let r = resources(&mut j, dst.clone(), Some(dpb.clone()), true, true, &[]);
        unsafe { rt.execute(&j, r) }.unwrap();
        assert_eq!(dst.writes.load(Ordering::SeqCst), 1);
        assert_eq!(dpb.writes.load(Ordering::SeqCst), 1);
        let mut next = job(owner.clone(), 1);
        next.references = vec![reference(0)];
        let mut wrong = resources(&mut next, image(), Some(image()), false, false, &[]);
        wrong.references.push(dst);
        assert!(
            unsafe { rt.execute(&next, wrong) }
                .unwrap_err()
                .contains("identity")
        );
        next.references.clear();
        let missing = resources(&mut next, image(), Some(image()), false, false, &[]);
        assert!(
            unsafe { rt.execute(&next, missing) }
                .unwrap_err()
                .contains("undeclared")
        );
        next.references = vec![reference(0)];
        let mut valid = resources(&mut next, image(), Some(image()), false, false, &[]);
        valid.references.push(dpb);
        unsafe { rt.execute(&next, valid) }.unwrap();
    }
    #[test]
    fn begin_rebinds_only_a_known_canonical_generation() {
        let mut rt = Runtime::default();
        let owner = session();
        let mut j = job(owner.clone(), 0);
        let dpb = image();
        let r = resources(&mut j, image(), Some(dpb.clone()), true, true, &[]);
        unsafe { rt.execute(&j, r) }.unwrap();
        let mut next = job(owner, 1);
        let mut known = reference(0);
        known.slot = 2;
        next.begin_before = true;
        next.begin_references = vec![known.clone()];
        next.references = vec![known];
        let mut r = resources(&mut next, image(), Some(image()), false, false, &[]);
        r.begin_references.push(dpb.clone());
        r.references.push(dpb);
        unsafe { rt.execute(&next, r) }.unwrap();
    }
    #[test]
    fn nonreference_setup_never_resolves_or_writes_and_invalidates_slot() {
        let mut rt = Runtime::default();
        let owner = session();
        let mut j = job(owner.clone(), 0);
        let dpb = image();
        let r = resources(&mut j, image(), Some(dpb.clone()), true, true, &[]);
        unsafe { rt.execute(&j, r) }.unwrap();
        let mut nonref = job(owner.clone(), 1);
        nonref.h264.is_reference = false;
        nonref.h264.pic_order_cnt = [1; 2];
        nonref.setup.as_mut().unwrap().picture.layer = 999;
        nonref.setup.as_mut().unwrap().picture.extent.width = 0;
        let r = resources(&mut nonref, image(), None, false, true, &[]);
        unsafe { rt.execute(&nonref, r) }.unwrap();
        assert_eq!(dpb.writes.load(Ordering::SeqCst), 1);
        let mut next = job(owner, 1);
        next.references = vec![reference(0)];
        let mut r = resources(&mut next, image(), Some(image()), false, false, &[]);
        r.references.push(dpb);
        assert!(
            unsafe { rt.execute(&next, r) }
                .unwrap_err()
                .contains("unknown native DPB slot")
        );
    }
    #[test]
    fn accepted_copy_failure_poison_is_sticky() {
        let mut rt = Runtime::default();
        let mut j = job(session(), 0);
        let fail = Arc::new(Image {
            fail: true,
            ..Default::default()
        });
        let r = resources(&mut j, fail, Some(image()), true, true, &[]);
        assert!(unsafe { rt.execute(&j, r) }.is_err());
        let r = resources(&mut j, image(), Some(image()), true, true, &[]);
        assert!(
            unsafe { rt.execute(&j, r) }
                .unwrap_err()
                .contains("poisoned")
        );
    }
    #[test]
    fn mmco5_requires_source_marking_and_normalized_setup() {
        let mut rt = Runtime::default();
        let owner = session();
        let mut j = job(owner.clone(), 0);
        let r = resources(&mut j, image(), Some(image()), true, true, &[]);
        unsafe { rt.execute(&j, r) }.unwrap();
        let mut next = job(owner, 1);
        next.setup.as_mut().unwrap().h264.frame_num = 0;
        next.setup.as_mut().unwrap().h264.pic_order_cnt = [0; 2];
        let wrong = resources(&mut next, image(), Some(image()), false, true, &[]);
        assert!(
            unsafe { rt.execute(&next, wrong) }
                .unwrap_err()
                .contains("marking")
        );
        let r = resources(&mut next, image(), Some(image()), false, true, &[(5, 0, 0)]);
        unsafe { rt.execute(&next, r) }.unwrap();
    }
    #[test]
    fn mmco3_promotion_preserves_original_canonical_image() {
        let mut rt = Runtime::default();
        let owner = session();
        let old = image();
        let mut first = job(owner.clone(), 0);
        let r = resources(&mut first, image(), Some(old.clone()), true, true, &[]);
        unsafe { rt.execute(&first, r) }.unwrap();
        let mut second = job(owner.clone(), 1);
        second.setup.as_mut().unwrap().slot = 1;
        let r = resources(
            &mut second,
            image(),
            Some(image()),
            false,
            true,
            &[(4, 2, 0), (3, 0, 1)],
        );
        unsafe { rt.execute(&second, r) }.unwrap();
        let mut third = job(owner, 2);
        third.begin_before = true;
        let mut promoted = reference(0);
        promoted.h264.long_term = true;
        promoted.h264.frame_num = 1;
        third.begin_references = vec![promoted.clone()];
        third.references = vec![promoted];
        // P defaults still select newer short-term frame1, so declaring only the
        // promoted older LT cannot satisfy the actual list.
        let mut r = resources(&mut third, image(), Some(image()), false, false, &[]);
        r.begin_references.push(old.clone());
        r.references.push(old.clone());
        assert!(
            unsafe { rt.execute(&third, r) }
                .unwrap_err()
                .contains("undeclared")
        );
        // Explicit list0 idc2 selects that exact promoted generation. The
        // untouched original image and updated LT index must now succeed.
        let mut b = Writer::default();
        b.ue(0);
        b.ue(0);
        b.ue(0);
        b.fixed(2, 4);
        b.bit(false); // active count defaults to one
        b.bit(true);
        b.ue(2);
        b.ue(1);
        b.ue(3);
        b.bit(false); // sliding marking
        let mut bytes = vec![0, 0, 1];
        bytes.extend(b.nal(0x41));
        third.source_range = bytes.len() as u64;
        let r = Resources {
            source_range: 0..bytes.len(),
            source: Arc::new(Bytes(bytes)),
            destination: image(),
            setup: Some(image()),
            references: vec![old.clone()],
            begin_references: vec![old],
        };
        unsafe { rt.execute(&third, r) }.unwrap();
    }
    #[test]
    fn overwritten_canonical_image_cannot_be_rebound() {
        let mut rt = Runtime::default();
        let owner = session();
        let canonical = image();
        let mut first = job(owner.clone(), 0);
        let r = resources(
            &mut first,
            image(),
            Some(canonical.clone()),
            true,
            true,
            &[],
        );
        unsafe { rt.execute(&first, r) }.unwrap();
        // Nonref output overwrites old DPB pixels, despite VT retaining a codec
        // reference internally. The old generation loses its canonical resource.
        let mut second = job(owner.clone(), 1);
        second.h264.is_reference = false;
        second.h264.pic_order_cnt = [1; 2];
        second.setup = None;
        let r = resources(&mut second, canonical.clone(), None, false, true, &[]);
        unsafe { rt.execute(&second, r) }.unwrap();
        let mut third = job(owner, 1);
        third.begin_before = true;
        third.begin_references = vec![reference(0)];
        let mut r = resources(&mut third, image(), Some(image()), false, true, &[]);
        r.begin_references.push(canonical);
        assert!(
            unsafe { rt.execute(&third, r) }
                .unwrap_err()
                .contains("identity")
        );
    }
    #[test]
    fn original_baseline_eight_frames_keep_source_poc_and_idr_identity() {
        let bytes = include_bytes!("../examples/assets/videotoolbox-h264/progressive-64x50.h264");
        let mut nals = Vec::new();
        let mut at = 0;
        while at + 3 < bytes.len() {
            let prefix = if bytes[at..].starts_with(&[0, 0, 0, 1]) {
                4
            } else if bytes[at..].starts_with(&[0, 0, 1]) {
                3
            } else {
                at += 1;
                continue;
            };
            nals.push((at, at + prefix));
            at += prefix;
        }
        let mut sps = Vec::new();
        let mut pps = Vec::new();
        let mut frames = Vec::new();
        for i in 0..nals.len() {
            let (start, payload) = nals[i];
            let end = nals.get(i + 1).map_or(bytes.len(), |n| n.0);
            match bytes[payload] & 31 {
                7 => sps = bytes[payload..end].to_vec(),
                8 => pps = bytes[payload..end].to_vec(),
                1 | 5 => frames.push(start..end),
                _ => {}
            }
        }
        assert_eq!(frames.len(), 8);
        let owner = session();
        let mut rt = Runtime::default();
        let mut prior = None;
        for (i, range) in frames.into_iter().enumerate() {
            let frame = (i % 4) as u16;
            let mut j = job(owner.clone(), frame);
            j.configuration = Configuration {
                sps: sps.clone(),
                pps: pps.clone(),
            };
            j.source_range = range.len() as u64;
            let target = image();
            let mut references = Vec::new();
            if frame != 0 {
                j.references = vec![reference(frame - 1)];
                references.push(prior.take().unwrap());
            }
            let r = Resources {
                source: Arc::new(Bytes(bytes.to_vec())),
                source_range: range,
                destination: image(),
                setup: Some(target.clone()),
                references,
                begin_references: vec![],
            };
            unsafe { rt.execute(&j, r) }.unwrap();
            prior = Some(target);
        }
    }
    #[test]
    fn actual_source_flag_mismatch_is_rejected_before_native_write() {
        let mut j = job(session(), 0);
        let dst = image();
        let mut r = resources(&mut j, dst.clone(), Some(image()), true, true, &[]);
        j.h264.is_reference = false;
        r.setup = None;
        assert!(
            unsafe { Runtime::default().execute(&j, r) }
                .unwrap_err()
                .contains("flags")
        );
        assert_eq!(dst.writes.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn original_non_idr_i_bootstraps_with_absent_stock_intra_flag() {
        let bytes = include_bytes!("../examples/assets/videotoolbox-h264/non-idr-intra-64x50.h264");
        let mut j = job(session(), 3);
        j.h264.pic_order_cnt = [8; 2];
        j.source_range = 121;
        j.configuration = Configuration {
            sps: bytes[4..26].to_vec(),
            pps: bytes[30..35].to_vec(),
        };
        j.setup.as_mut().unwrap().h264.pic_order_cnt = [8; 2];
        let dst = image();
        let dpb = image();
        let mut rt = Runtime::default();
        for reset in [false, true] {
            j.reset_before = reset;
            let r = Resources {
                source: Arc::new(Bytes(bytes.to_vec())),
                source_range: 35..156,
                destination: dst.clone(),
                setup: Some(dpb.clone()),
                references: vec![],
                begin_references: vec![],
            };
            unsafe { rt.execute(&j, r) }.unwrap();
        }
        assert_eq!(dst.writes.load(Ordering::SeqCst), 2);
    }
}
