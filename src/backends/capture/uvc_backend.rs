use crate::{CameraFormat, CameraInfo, CaptureBackendTrait, FrameFormat, NokhwaError, Resolution};
use flume::{Receiver, Sender};
use image::{ImageBuffer, Rgb};
use ouroboros::self_referencing;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    mem::MaybeUninit,
    sync::{atomic::AtomicUsize, Arc},
};
use uvc::{
    ActiveStream, Context, Device, DeviceHandle, FrameFormat as UVCFrameFormat, StreamFormat,
    StreamHandle,
};

#[cfg(feature = "input_uvc")]
impl From<FrameFormat> for UVCFrameFormat {
    fn from(ff: FrameFormat) -> Self {
        match ff {
            FrameFormat::MJPEG => UVCFrameFormat::MJPEG,
            FrameFormat::YUYV => UVCFrameFormat::YUYV,
        }
    }
}

#[cfg(feature = "input_uvc")]
impl From<CameraFormat> for StreamFormat {
    fn from(cf: CameraFormat) -> Self {
        StreamFormat {
            width: cf.width(),
            height: cf.height(),
            fps: cf.framerate(),
            format: cf.format().into(),
        }
    }
}

// ignore the IDE, this compiles
/// The backend struct that interfaces with libuvc.
/// To see what this does, please see [`CaptureBackendTrait`]
/// # Quirks
/// - The indexing for this backend is based off of `libuvc`'s device ordering, not the OS.
/// - You must call [create()](UVCCaptureDevice::create()) instead `new()`, some methods are auto-generated by the self-referencer and are not meant to be used.
/// - The [create()](UVCCaptureDevice::create()) method will open the device twice.
/// - Calling [`set_resolution()`](CaptureBackendTrait::set_resolution()), [`set_framerate()`](CaptureBackendTrait::set_framerate()), or [`set_frameformat()`](CaptureBackendTrait::set_frameformat()) each internally calls [`set_camera_format()`](CaptureBackendTrait::set_camera_format()).
/// - [`get_frame_raw()`](CaptureBackendTrait::get_frame_raw()) returns the same raw data as [`get_frame()`](CaptureBackendTrait::get_frame()), a.k.a. no custom decoding required, all data is automatically RGB
/// This backend, once stream is open, will constantly collect frames. When you call [`get_frame()`](CaptureBackendTrait::get_frame()) or one of its variants, it will only give you the latest frame.
/// # Safety
/// This backend requires use of `unsafe` due to the self-referencing structs involved.
/// If [`open_stream()`](CaptureBackendTrait::open_stream()) and [`get_frame()`](CaptureBackendTrait::get_frame()) are called in the wrong order this may crash the entire program.
#[self_referencing(chain_hack)]
pub struct UVCCaptureDevice<'a> {
    camera_format: CameraFormat,
    camera_info: CameraInfo,
    frame_receiver: Box<Receiver<Vec<u8>>>,
    frame_sender: Box<Sender<Vec<u8>>>,
    stream_handle_init: Cell<bool>,
    active_stream_init: Cell<bool>,
    context: Box<Context<'a>>,
    #[borrows(context)]
    #[not_covariant]
    device: Box<Device<'this>>,
    #[borrows(device)]
    #[not_covariant]
    device_handle: Box<DeviceHandle<'this>>,
    #[borrows(device_handle)]
    #[not_covariant]
    stream_handle: Box<RefCell<MaybeUninit<StreamHandle<'this>>>>,
    #[borrows(stream_handle)]
    #[not_covariant]
    active_stream: Box<RefCell<MaybeUninit<ActiveStream<'this, Arc<AtomicUsize>>>>>,
}

impl<'a> UVCCaptureDevice<'a> {
    /// Creates a UVC Camera device with optional [`CameraFormat`].
    /// If `camera_format` is `None`, it will be spawned with with 640x480@15 FPS, MJPEG [`CameraFormat`] default.
    /// # Panics
    /// This operation may panic! If the UVC Context fails to retrieve the device from the gotten IDs, this operation will panic.
    /// # Errors
    /// This may error when the `libuvc` backend fails to retreive the device or its data.
    pub fn create(index: usize, cam_fmt: Option<CameraFormat>) -> Result<Self, NokhwaError> {
        let context = match Context::new() {
            Ok(ctx) => Box::new(ctx),
            Err(why) => return Err(NokhwaError::CouldntOpenDevice(why.to_string())),
        };

        let (camera_info, frame_receiver, frame_sender, vendor_id, product_id, serial) = {
            let device_list = match context.devices() {
                Ok(device_list) => device_list,
                Err(why) => return Err(NokhwaError::CouldntOpenDevice(why.to_string())),
            };

            let device = match device_list.into_iter().nth(index) {
                Some(d) => Box::new(d),
                None => {
                    return Err(NokhwaError::CouldntOpenDevice(format!(
                        "Device at {} not found",
                        index
                    )))
                }
            };

            let device_desc = match device.description() {
                Ok(desc) => desc,
                Err(why) => return Err(NokhwaError::CouldntOpenDevice(why.to_string())),
            };

            let device_name = match (device_desc.manufacturer, device_desc.product) {
                (Some(manu), Some(prod)) => {
                    format!("{} {}", manu, prod)
                }
                (_, Some(prod)) => prod,
                (Some(manu), _) => {
                    format!(
                        "{}:{} {}",
                        device_desc.vendor_id, device_desc.product_id, manu
                    )
                }
                (_, _) => {
                    format!("{}:{}", device_desc.vendor_id, device_desc.product_id)
                }
            };

            let camera_info = CameraInfo::new(
                device_name,
                "".to_string(),
                format!("{}:{}", device_desc.vendor_id, device_desc.product_id),
                index,
            );

            let (vendor_id, product_id, serial) = (
                Some(i32::from(device_desc.product_id)),
                Some(i32::from(device_desc.vendor_id)),
                device_desc.serial_number,
            );

            let (frame_sender, frame_receiver) = {
                let (a, b) = flume::unbounded::<Vec<u8>>();
                (Box::new(a), Box::new(b))
            };
            (
                camera_info,
                frame_receiver,
                frame_sender,
                vendor_id,
                product_id,
                serial,
            )
        };

        let camera_format = match cam_fmt {
            Some(cfmt) => cfmt,
            None => CameraFormat::default(),
        };

        Ok(UVCCaptureDeviceBuilder {
            camera_format,
            camera_info,
            frame_receiver,
            frame_sender,
            context,
            stream_handle_init: Cell::new(false),
            active_stream_init: Cell::new(false),
            device_builder: |context_builder| {
                Box::new(
                    context_builder
                        .find_device(vendor_id, product_id, serial.as_deref())
                        .unwrap(),
                )
            },
            device_handle_builder: |device_builder| Box::new(device_builder.open().unwrap()),
            stream_handle_builder: |_device_handle_builder| {
                Box::new(RefCell::new(MaybeUninit::uninit()))
            },
            active_stream_builder: |_stream_handle_builder| {
                Box::new(RefCell::new(MaybeUninit::uninit()))
            },
        }
        .build())
    }

    /// Create a UVC Camera with desired settings.
    /// # Panics
    /// This operation may panic! If the UVC Context fails to retrieve the device from the gotten IDs, this operation will panic.
    /// # Errors
    /// This may error when the `libuvc` backend fails to retreive the device or its data.
    pub fn create_with(
        index: usize,
        width: u32,
        height: u32,
        fps: u32,
        fourcc: FrameFormat,
    ) -> Result<Self, NokhwaError> {
        let camera_format = Some(CameraFormat::new_from(width, height, fourcc, fps));
        UVCCaptureDevice::create(index, camera_format)
    }
}

// IDE Autocomplete ends here. Do not be afraid it your IDE does not show completion.
// Here are some docs to help you out: https://docs.rs/ouroboros/0.9.3/ouroboros/attr.self_referencing.html
impl<'a> CaptureBackendTrait for UVCCaptureDevice<'a> {
    fn get_info(&self) -> CameraInfo {
        self.borrow_camera_info().clone()
    }

    fn get_camera_format(&self) -> CameraFormat {
        *self.borrow_camera_format()
    }

    fn get_compatible_list_by_resolution(
        &self,
        fourcc: FrameFormat,
    ) -> Result<HashMap<Resolution, Vec<u32>>, NokhwaError> {
        todo!()
    }

    fn get_resolution_list(&self, fourcc: FrameFormat) -> Result<Vec<Resolution>, NokhwaError> {
        todo!()
    }

    fn set_camera_format(&mut self, new_fmt: CameraFormat) -> Result<(), NokhwaError> {
        let prev_fmt = *self.borrow_camera_format();

        self.with_camera_format_mut(|cfmt| {
            *cfmt = new_fmt;
        });

        let is_streamh_some = self.borrow_stream_handle_init().get();

        if is_streamh_some {
            return match self.open_stream() {
                Ok(_) => Ok(()),
                Err(why) => {
                    // revert
                    self.with_camera_format_mut(|cfmt| {
                        *cfmt = prev_fmt;
                    });
                    Err(NokhwaError::CouldntSetProperty {
                        property: "CameraFormat".to_string(),
                        value: new_fmt.to_string(),
                        error: why.to_string(),
                    })
                }
            };
        }
        Ok(())
    }

    fn get_resolution(&self) -> Resolution {
        self.borrow_camera_format().resoltuion()
    }

    fn set_resolution(&mut self, new_res: Resolution) -> Result<(), NokhwaError> {
        todo!()
    }

    fn get_framerate(&self) -> u32 {
        self.borrow_camera_format().framerate()
    }

    fn set_framerate(&mut self, new_fps: u32) -> Result<(), NokhwaError> {
        todo!()
    }

    fn get_frameformat(&self) -> FrameFormat {
        self.borrow_camera_format().format()
    }

    fn set_frameformat(&mut self, fourcc: FrameFormat) -> Result<(), NokhwaError> {
        todo!()
    }

    fn open_stream(&mut self) -> Result<(), NokhwaError> {
        let ret: Result<(), NokhwaError> = self.with_mut(|fields| {
            let stream_format: StreamFormat = CameraFormat::into(*fields.camera_format);

            // first, drop the existing stream by setting it to None
            {
                if fields.active_stream_init.get() {
                    let innard_value = fields.active_stream.replace(MaybeUninit::uninit());
                    unsafe { std::mem::drop(innard_value.assume_init()) };
                    fields.active_stream_init.set(false);
                }

                if fields.stream_handle_init.get() {
                    let innard_value = fields.stream_handle.replace(MaybeUninit::uninit());
                    unsafe { std::mem::drop(innard_value.assume_init()) };
                    fields.stream_handle_init.set(false);
                }
            }
            // second, set the stream handle according to the streamformat
            match fields
                .device_handle
                .get_stream_handle_with_format(stream_format)
            {
                Ok(streamh) => match fields.stream_handle.try_borrow_mut() {
                    Ok(mut streamh_raw) => {
                        *streamh_raw = MaybeUninit::new(streamh);
                        fields.stream_handle_init.set(true);
                    }
                    Err(why) => return Err(NokhwaError::CouldntOpenStream(why.to_string())),
                },
                Err(why) => return Err(NokhwaError::CouldntOpenStream(why.to_string())),
            }
            Ok(())
        });

        if ret.is_err() {
            return ret;
        }

        let ret_2: Result<(), NokhwaError> = self.with(|fields| {
            // finally, get the active stream
            let counter = Arc::new(AtomicUsize::new(0));
            let frame_sender: Sender<Vec<u8>> = *(self.with_frame_sender(|send| send)).clone();
            let streamh = unsafe {
                let raw_ptr =
                    (*fields.stream_handle.borrow_mut()).as_ptr() as *mut MaybeUninit<StreamHandle>;
                let assume_inited: *mut MaybeUninit<StreamHandle<'static>> =
                    raw_ptr.cast::<MaybeUninit<uvc::StreamHandle>>();
                &mut *assume_inited
            };
            let streamh_init = unsafe {
                match streamh.as_mut_ptr().as_mut() {
                    Some(sth) => sth,
                    None => {
                        return Err(NokhwaError::CouldntOpenStream(
                            "Failed to get mutable raw pointer to stream handle!".to_string(),
                        ))
                    }
                }
            };

            let active_stream = match streamh_init.start_stream(
                move |frame, _count| {
                    let vec_frame: Vec<u8> = frame.to_rgb().unwrap().to_bytes().to_vec();
                    if frame_sender.send(vec_frame).is_err() {
                        // do nothing
                    }
                },
                counter,
            ) {
                Ok(active) => active,
                Err(why) => return Err(NokhwaError::CouldntOpenStream(why.to_string())),
            };
            *fields.active_stream.borrow_mut() = MaybeUninit::new(active_stream);
            Ok(())
        });

        if ret_2.is_err() {
            return ret_2;
        }

        Ok(())
    }

    fn is_stream_open(&self) -> bool {
        self.with_active_stream_init(Cell::get)
    }

    fn get_frame(&mut self) -> Result<ImageBuffer<Rgb<u8>, Vec<u8>>, NokhwaError> {
        let data = match self.get_frame_raw() {
            Ok(d) => d,
            Err(why) => return Err(why),
        };

        let resolution: Resolution = self.borrow_camera_format().resoltuion();

        let imagebuf: ImageBuffer<Rgb<u8>, Vec<u8>> =
            match ImageBuffer::from_vec(resolution.width(), resolution.height(), data) {
                Some(img) => img,
                None => {
                    return Err(NokhwaError::CouldntCaptureFrame(
                        "ImageBuffer too small! This is probably a bug, please report it!"
                            .to_string(),
                    ))
                }
            };

        Ok(imagebuf)
    }

    fn get_frame_raw(&mut self) -> Result<Vec<u8>, NokhwaError> {
        // assertions
        if !self.borrow_active_stream_init().get() {
            return Err(NokhwaError::CouldntCaptureFrame(
                "Please call `open_stream()` first!".to_string(),
            ));
        }

        let f_recv = self.borrow_frame_receiver();
        let messages_iter = f_recv.drain();
        match messages_iter.last() {
            Some(msg) => Ok(msg),
            None => Err(NokhwaError::CouldntCaptureFrame("Too fast!".to_string())),
        }
    }

    fn stop_stream(&mut self) -> Result<(), NokhwaError> {
        self.with(|fields| {
            if fields.active_stream_init.get() {
                let innard_value = fields.active_stream.replace(MaybeUninit::uninit());
                unsafe { std::mem::drop(innard_value.assume_init()) };
                fields.active_stream_init.set(false);
            }

            if fields.stream_handle_init.get() {
                let innard_value = fields.stream_handle.replace(MaybeUninit::uninit());
                unsafe { std::mem::drop(innard_value.assume_init()) };
                fields.stream_handle_init.set(false);
            }
        });
        Ok(())
    }
}
