use anyhow::Error;
use derive_more::{Display, Error};
use gst::{glib, prelude::*};
use gstreamer as gst;
use gstreamer_app as gst_app;
use log::*;
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

mod klv;
mod run;

#[derive(Debug, Display, Error)]
#[display(fmt = "Received error from {src}: {error} (debug: {debug:?})")]
struct ErrorMessage {
    src: glib::GString,
    error: glib::Error,
    debug: Option<glib::GString>,
}

fn video_with_klv() -> Result<gst::Pipeline, Error> {
    gst::init().unwrap();
    let pipeline = gst::Pipeline::new();
    let videosrc = gst::ElementFactory::make("avfvideosrc").build().unwrap();
    let x264enc = gst::ElementFactory::make("x264enc").build().unwrap();
    x264enc.set_property_from_str("tune", "zerolatency");

    let h264parse = gst::ElementFactory::make("h264parse").build().unwrap();
    let mpegtsmux = gst::ElementFactory::make("mpegtsmux").build().unwrap();
    let tsdemux = gst::ElementFactory::make("tsdemux").build().unwrap();
    //let tee = gst::ElementFactory::make("tee").build().unwrap();

    let h264parse_dest = gst::ElementFactory::make("h264parse").build().unwrap();
    let avdec_h264 = gst::ElementFactory::make("avdec_h264").build().unwrap();
    let videoconvert = gst::ElementFactory::make("videoconvert").build().unwrap();
    let videosink = gst::ElementFactory::make("osxvideosink").build().unwrap();
    videosink.set_property_from_str("sync", "false");

    let appsrc = klv::klv_test_src().unwrap();
    let appsink = klv::klv_sink().unwrap();

    pipeline
        .add_many(&[
            &appsrc,
            &videosrc,
            &h264parse,
            &x264enc,
            &mpegtsmux,
            //&tee,
            &tsdemux,
            &h264parse_dest,
            &avdec_h264,
            &videoconvert,
            &videosink,
        ])
        .unwrap();

    // link video source to mpegtsmux
    //videosrc.link_filtered(&x264enc, &videosrc_caps).unwrap();
    videosrc.link(&x264enc).unwrap();
    x264enc.link(&h264parse).unwrap();
    h264parse.link(&mpegtsmux).unwrap();
    appsrc
        .link_filtered(
            &mpegtsmux,
            &gst::Caps::builder("meta/x-klv")
                .field("parsed", true)
                .build(),
        )
        .unwrap();

    mpegtsmux.link(&tsdemux).unwrap();
    //mpegtsmux.set_property("alignment", 7);

    // link display pipe
    gst::Element::link_many(&[&h264parse_dest, &avdec_h264, &videoconvert, &videosink]).unwrap();
    let h264_sink_pad = h264parse_dest
        .static_pad("sink")
        .expect("h264 could not be linked.");

    // Pipeline can be disposed of at any point (), so convert to a weak ref that will force us to check if there is any strong reference
    // using `pipeline_weak.upgrade()` below
    let pipeline_weak = pipeline.downgrade();
    // Demuxer needs to connect after playing (detect source)
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
            let elements = &[&queue, &appsink];
            pipeline
                .add_many(elements)
                .expect("failed to add audio elements to pipeline");
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

    let srcpad = videosrc.static_pad("src").unwrap();
    let ts = Arc::new(Mutex::new(Instant::now()));

    srcpad.add_probe(gst::PadProbeType::DATA_DOWNSTREAM, move |_, probe_info| {
        match probe_info.data {
            Some(gst::PadProbeData::Event(ref event)) => {
                info!("Event {:?}", event);
            }
            Some(gst::PadProbeData::Buffer(ref buf)) => {
                let now = Instant::now();
                let mut ts = ts.lock().unwrap();
                let frame_time_ms = (now - *ts).as_micros() as f32 / 1000.;
                *ts = now;
                let fram_time = buf.pts();

                if frame_time_ms > 35. {
                    warn!("frame {:?} {}", fram_time, frame_time_ms);
                } else {
                    info!("frame {:?} {}", fram_time, frame_time_ms);
                }

                // Example KLV set taken from: https://en.wikipedia.org/wiki/KLV#Example
                let data = [0x2A, 0x02, 0x00, 0x03];

                if let Some(appsrc) = appsrc.downcast_ref::<gst_app::AppSrc>() {
                    let mut buffer = gst::Buffer::with_size(data.len()).unwrap();
                    {
                        let bufref = buffer.make_mut();
                        bufref.set_pts(fram_time);
                        bufref.set_dts(buf.dts());

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
