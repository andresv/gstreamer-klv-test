use anyhow::Error;
use derive_more::{Display, Error};
use gst::{glib, prelude::*};
use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use log::*;
use std::ops;
use std::{
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};
//use pango::prelude::*;
use pango::prelude::{FontMapExt, ObjectExt as _};

mod klv;
mod run;

#[derive(Debug, Display, Error)]
#[display(fmt = "Received error from {src}: {error} (debug: {debug:?})")]
struct ErrorMessage {
    src: glib::GString,
    error: glib::Error,
    debug: Option<glib::GString>,
}

struct DrawingContext {
    layout: LayoutWrapper,
    info: Option<gst_video::VideoInfo>,
}

#[derive(Debug)]
struct LayoutWrapper(pango::Layout);

impl ops::Deref for LayoutWrapper {
    type Target = pango::Layout;

    fn deref(&self) -> &pango::Layout {
        assert_eq!(self.0.ref_count(), 1);
        &self.0
    }
}

// SAFETY: We ensure that there are never multiple references to the layout.
unsafe impl Send for LayoutWrapper {}

fn video_with_klv() -> Result<gst::Pipeline, Error> {
    gst::init()?;
    let pipeline = gst::Pipeline::new();
    let videosrc = gst::ElementFactory::make("avfvideosrc").build()?;
    let x264enc = gst::ElementFactory::make("x264enc").build()?;
    x264enc.set_property_from_str("tune", "zerolatency");

    let h264parse = gst::ElementFactory::make("h264parse").build()?;
    let mpegtsmux = gst::ElementFactory::make("mpegtsmux").build()?;
    let tsdemux = gst::ElementFactory::make("tsdemux").build()?;

    let h264parse_dest = gst::ElementFactory::make("h264parse").build()?;
    let avdec_h264 = gst::ElementFactory::make("avdec_h264").build()?;
    let videoconvert = gst::ElementFactory::make("videoconvert").build()?;
    let overlay = gst::ElementFactory::make("overlaycomposition").build()?;
    // Plug in a capsfilter element that will force the videotestsrc and the overlay to work
    // with images of the size 800x800, and framerate of 15 fps, since my laptop struggles
    // rendering it at the default 30 fps
    let caps = gst_video::VideoCapsBuilder::new()
        .width(1920)
        .height(1080)
        .framerate((30, 1).into())
        .build();
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .property("caps", &caps)
        .build()?;

    let videosink = gst::ElementFactory::make("osxvideosink").build()?;
    videosink.set_property_from_str("sync", "false");

    let appsrc = klv::klv_test_src()?;
    let appsink = klv::klv_sink()?;

    pipeline.add_many(&[
        &appsrc,
        &videosrc,
        &h264parse,
        &x264enc,
        &mpegtsmux,
        //&tee,
        &tsdemux,
        &h264parse_dest,
        &avdec_h264,
        &overlay,
        &capsfilter,
        &videoconvert,
        &videosink,
    ])?;

    videosrc.link(&x264enc)?;
    x264enc.link(&h264parse)?;
    h264parse.link(&mpegtsmux)?;
    // h264 video and KLV stream are both linked to mpegtsmux which muxes them together.
    appsrc
        .link_filtered(
            &mpegtsmux,
            &gst::Caps::builder("meta/x-klv")
                .field("parsed", true)
                .build(),
        )
        .unwrap();

    // For demonstration purposes `tsdemux` takes video stream and klv stream again apart.
    mpegtsmux.link(&tsdemux).unwrap();

    // Link display pipe.
    gst::Element::link_many(&[
        &h264parse_dest,
        &avdec_h264,
        &overlay,
        &capsfilter,
        &videoconvert,
        &videosink,
    ])
    .unwrap();
    let h264_sink_pad = h264parse_dest
        .static_pad("sink")
        .expect("h264 could not be linked.");

    let klv_sink_pad = appsink.static_pad("sink").unwrap();
    let video_sink_pad = videosink.static_pad("sink").unwrap();

    // The PangoFontMap represents the set of fonts available for a particular rendering system.
    let fontmap = pangocairo::FontMap::new();
    // Create a new pango layouting context for the fontmap.
    let context = fontmap.create_context();
    // Create a pango layout object. This object is a string of text we want to layout.
    // It is wrapped in a LayoutWrapper (defined above) to be able to send it across threads.
    let layout = LayoutWrapper(pango::Layout::new(&context));

    // Select the text content and the font we want to use for the piece of text.
    let font_desc = pango::FontDescription::from_string("monospace 26");
    layout.set_font_description(Some(&font_desc));
    layout.set_text("GStreamer");

    // The following is a context struct (containing the pango layout and the configured video info).
    // We have to wrap it in an Arc (or Rc) to get reference counting, that is: to be able to have
    // shared ownership of it in multiple different places (the two signal handlers here).
    // We have to wrap it in a Mutex because Rust's type-system can't know that both signals are
    // only ever called from a single thread (the streaming thread). It would be enough to have
    // something that is Send in theory but that's not how signal handlers are generated unfortunately.
    // The Mutex (or otherwise if we didn't need the Sync bound we could use a RefCell) is to implement
    // interior mutability (see Rust docs). Via this we can get a mutable reference to the contained
    // data which is checked at runtime for uniqueness (blocking in case of mutex, panic in case
    // of refcell) instead of compile-time (like with normal references).
    let drawer = Arc::new(Mutex::new(DrawingContext { layout, info: None }));
    let latest_klv: Arc<Mutex<Option<gst::Buffer>>> = Arc::new(Mutex::new(None));
    let latest_klv2 = Arc::clone(&latest_klv);
    // Connect to the overlaycomposition element's "draw" signal, which is emitted for
    // each videoframe piped through the element. The signal handler needs to
    // return a gst_video::VideoOverlayComposition to be drawn on the frame
    //
    // Signals connected with the connect(<name>, ...) API get their arguments
    // passed as array of glib::Value. For a documentation about the actual arguments
    // it is always a good idea to check the element's signals using either
    // gst-inspect, or the online documentation.
    //
    // In this case, the signal passes the gst::Element and a gst::Sample with
    // the current buffer
    overlay.connect_closure(
        "draw",
        false,
        glib::closure!(@strong drawer => move |_overlay: &gst::Element,
                                               sample: &gst::Sample| {
            let drawer = drawer.lock().unwrap();

            let buffer = sample.buffer().unwrap();
            let timestamp = buffer.pts().unwrap();

            let info = drawer.info.as_ref().unwrap();
            let layout = &drawer.layout;

            // Create a Cairo image surface to draw into and the context around it.
            let surface = cairo::ImageSurface::create(
                cairo::Format::ARgb32,
                info.width() as i32,
                info.height() as i32,
            )
            .unwrap();
            let cr = cairo::Context::new(&surface).expect("Failed to create cairo context");

            cr.save().expect("Failed to save state");
            cr.set_operator(cairo::Operator::Clear);
            cr.paint().expect("Failed to clear background");
            cr.restore().expect("Failed to restore state");

            // The image we draw (the text) will be static, but we will change the
            // transformation on the drawing context, which rotates and shifts everything
            // that we draw afterwards. Like this, we have no complicated calculations
            // in the actual drawing below.
            // Calling multiple transformation methods after each other will apply the
            // new transformation on top. If you repeat the cr.rotate(angle) line below
            // this a second time, everything in the canvas will rotate twice as fast.
            cr.translate(
                f64::from(info.width()) / 2.0,
                f64::from(info.height()) / 2.0,
            );

            // Cairo, like most rendering frameworks, is using a stack for transformations
            // with this, we push our current transformation onto this stack - allowing us
            // to make temporary changes / render something / and then returning to the
            // previous transformations.
            cr.save().expect("Failed to save state");
            cr.set_source_rgb(0.90, 0.65, 0.36);

            // Update the text layout. This function is only updating pango's internal state.
            // So e.g. that after a 90 degree rotation it knows that what was previously going
            // to end up as a 200x100 rectangle would now be 100x200.
            pangocairo::functions::update_layout(&cr, layout);
            let (width, _height) = layout.size();
            // Using width and height of the text, we can properly position it within
            // our canvas.
            cr.move_to(
                -(f64::from(width) / f64::from(pango::SCALE)) / 2.0,
                -(f64::from(info.height())) / 8.0,
            );

            let latest_klv = latest_klv.lock().unwrap();
            let klv_ts = if let Some(buf) = latest_klv.as_ref() {
                format!("{}", buf.pts().unwrap())
            } else {
                String::from("not available")
            };
            layout.set_text(&format!("frame time: {timestamp}\n  klv time: {klv_ts}"));
            // After telling the layout object where to draw itself, we actually tell
            // it to draw itself into our cairo context.
            pangocairo::functions::show_layout(&cr, layout);

            // Here we go one step up in our stack of transformations, removing any
            // changes we did to them since the last call to cr.save();
            cr.restore().expect("Failed to restore state");


            /* Drop the Cairo context to release the additional reference to the data and
             * then take ownership of the data. This only works if we have the one and only
             * reference to the image surface */
            drop(cr);
            let stride = surface.stride();
            let data = surface.take_data().unwrap();

            /* Create an RGBA buffer, and add a video meta that the videooverlaycomposition expects */
            let mut buffer = gst::Buffer::from_mut_slice(data);

            gst_video::VideoMeta::add_full(
                buffer.get_mut().unwrap(),
                gst_video::VideoFrameFlags::empty(),
                gst_video::VideoFormat::Bgra,
                info.width(),
                info.height(),
                &[0],
                &[stride],
            )
            .unwrap();

            /* Turn the buffer into a VideoOverlayRectangle, then place
             * that into a VideoOverlayComposition and return it.
             *
             * A VideoOverlayComposition can take a Vec of such rectangles
             * spaced around the video frame, but we're just outputting 1
             * here */
            let rect = gst_video::VideoOverlayRectangle::new_raw(
                &buffer,
                0,
                0,
                info.width(),
                info.height(),
                gst_video::VideoOverlayFormatFlags::PREMULTIPLIED_ALPHA,
            );

            gst_video::VideoOverlayComposition::new(Some(&rect))
                .unwrap()
        }),
    );

    // Add a signal handler to the overlay's "caps-changed" signal. This could e.g.
    // be called when the sink that we render to does not support resizing the image
    // itself - but the user just changed the window-size. The element after the overlay
    // will then change its caps and we use the notification about this change to
    // resize our canvas's size.
    // Another possibility for when this might happen is, when our video is a network
    // stream that dynamically changes resolution when enough bandwidth is available.
    overlay.connect_closure(
        "caps-changed",
        false,
        glib::closure!(move |_overlay: &gst::Element,
                             caps: &gst::Caps,
                             _width: u32,
                             _height: u32| {
            let mut drawer = drawer.lock().unwrap();
            drawer.info = Some(gst_video::VideoInfo::from_caps(caps).unwrap());
        }),
    );

    // Pipeline can be disposed of at any point (), so convert to a weak ref that will force us to check if there is any strong reference
    // using `pipeline_weak.upgrade()` below
    let pipeline_weak = pipeline.downgrade();

    // Demuxer needs to connect after playing (detect source).
    // It will create 2 srce pads: one for video and another for KLV metadata.
    // KLV src pad is connected to `appsink` through a `queue` element.
    tsdemux.connect_pad_added(move |src, src_pad| {
        if src_pad.name().contains("video") {
            info!(
                "connect new video pad {} from {}",
                src_pad.name(),
                src.name()
            );
            src_pad.link(&h264_sink_pad).unwrap();
        } else if src_pad.name().contains("private") {
            info!(
                "connect new metadata pad {} from {}",
                src_pad.name(),
                src.name()
            );
            let pipeline = match pipeline_weak.upgrade() {
                Some(pipeline) => pipeline,
                None => return,
            };

            let queue = gst::ElementFactory::make("queue").build().unwrap();
            //queue.set_property_from_str("max-size-buffers", "1");
            appsink.set_property_from_str("sync", "false");

            let elements = &[&queue, &appsink];
            pipeline
                .add_many(elements)
                .expect("failed to add elements to pipeline");
            gst::Element::link_many(elements).unwrap();
            let appsink_pad = queue
                .static_pad("sink")
                .expect("failed to get queue and appsink pad.");
            src_pad.link(&appsink_pad).unwrap();

            for e in elements {
                e.sync_state_with_parent().unwrap();
            }
        } else {
            warn!(
                "Received unsupported new pad {} from {}",
                src_pad.name(),
                src.name()
            );
        }
    });

    let video_src_pad = videosrc.static_pad("src").unwrap();
    let ts = Arc::new(Mutex::new(Instant::now()));
    let frame_nr = AtomicU32::new(0);

    // This is called evertime when new video frame is produced by videosrc.
    // Here KLV data is pushed to appsrc buffer.
    video_src_pad.add_probe(gst::PadProbeType::DATA_DOWNSTREAM, move |_, probe_info| {
        match probe_info.data {
            Some(gst::PadProbeData::Event(ref event)) => {
                info!("Event {:?}", event);
            }
            Some(gst::PadProbeData::Buffer(ref buf)) => {
                let now = Instant::now();
                let mut ts = ts.lock().unwrap();
                let frame_time_ms = (now - *ts).as_micros() as f32 / 1000.;
                *ts = now;
                let frame_time = buf.pts();

                // Example KLV set taken from: https://en.wikipedia.org/wiki/KLV#Example
                let data = [0x2A, 0x02, 0x00, 0x03];
                let nr = frame_nr.fetch_add(1, Ordering::SeqCst);
                let data = nr.to_le_bytes();

                if frame_time_ms > 35. {
                    error!("src frame {:?} {} {:?}", frame_time, frame_time_ms, data);
                } else {
                    warn!("src frame {:?} {} {:?}", frame_time, frame_time_ms, data);
                }

                if let Some(appsrc) = appsrc.downcast_ref::<gst_app::AppSrc>() {
                    let mut buffer = gst::Buffer::with_size(data.len()).unwrap();
                    {
                        let bufref = buffer.make_mut();
                        bufref.set_pts(frame_time);
                        //bufref.set_dts(buf.dts());

                        let mut mw = bufref.map_writable().unwrap();
                        mw.as_mut_slice().copy_from_slice(&data)
                    }

                    appsrc.push_buffer(buffer).unwrap();
                } else {
                    error!("Failed to downcast appsrc to gst_app::AppSrc");
                }
            }
            _ => (),
        }
        gst::PadProbeReturn::Ok
    });

    // Probe when KLV data reaches appsink element.
    klv_sink_pad.add_probe(gst::PadProbeType::DATA_DOWNSTREAM, {
        move |_, probe_info| {
            match probe_info.data {
                Some(gst::PadProbeData::Event(ref event)) => {
                    info!("Event {:?}", event);
                }
                Some(gst::PadProbeData::Buffer(ref buf)) => {
                    let mr = buf.map_readable().unwrap();
                    log::info!("klvprobe klv {:?} {:?}", buf.pts(), mr.as_slice());
                    let mut latest_klv2 = latest_klv2.lock().unwrap();
                    *latest_klv2 = Some(buf.clone());
                }
                _ => (),
            }
            gst::PadProbeReturn::Ok
        }
    });

    // Probe when new frame reaches videosink element.
    video_sink_pad.add_probe(gst::PadProbeType::DATA_DOWNSTREAM, move |_, probe_info| {
        match probe_info.data {
            Some(gst::PadProbeData::Event(ref event)) => {
                info!("Event {:?}", event);
            }
            Some(gst::PadProbeData::Buffer(ref buf)) => {
                log::info!("video sink {:?} ", buf.pts());
            }
            _ => (),
        }
        gst::PadProbeReturn::Ok
    });

    Ok(pipeline)
}

fn main_loop(pipeline: gst::Pipeline) -> Result<(), Error> {
    pipeline.set_state(gst::State::Playing)?;

    let bus = pipeline
        .bus()
        .expect("Pipeline without bus. Shouldn't happen!");

    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;

        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                error!(
                    "Error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
                break;
            }

            MessageView::StateChanged(s) => {
                info!(
                    "State changed from {:?}: {:?} -> {:?} ({:?})",
                    s.src().map(|s| s.path_string()),
                    s.old(),
                    s.current(),
                    s.pending()
                );
            }

            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null)?;

    Ok(())
}

fn main() {
    env_logger::builder().format_timestamp_millis().init();

    info!("start");
    run::run(|| match video_with_klv().and_then(main_loop) {
        Ok(r) => r,
        Err(e) => eprintln!("Error! {e}"),
    })
}
