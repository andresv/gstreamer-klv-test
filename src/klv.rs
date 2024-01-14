use anyhow::Result;
use gst::{element_error, prelude::*, Caps};
use gstreamer as gst;
use gstreamer_app as gst_app;

pub fn klv_sink() -> Result<gst::Element> {
    let appsink = gst_app::AppSink::builder()
        .caps(&Caps::builder("meta/x-klv").field("parsed", true).build())
        .build();

    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            // Add a handler to the "new-sample" signal.
            .new_sample(|appsink| {
                // Pull the sample in question out of the appsink's buffer.
                let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or_else(|| {
                    element_error!(
                        appsink,
                        gst::ResourceError::Failed,
                        ("Failed to get buffer from appsink")
                    );
                    gst::FlowError::Error
                })?;

                if buffer.size() > 0 {
                    let mr = buffer.map_readable().unwrap();
                    log::info!("receive klv {:?}", mr.as_slice());
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    Ok(appsink.upcast::<gst::Element>())
}

pub fn klv_test_src() -> Result<gst::Element> {
    let appsrc = gst_app::AppSrc::builder()
        .caps(&Caps::builder("meta/x-klv").field("parsed", true).build())
        .format(gst::Format::Time)
        .build();
    Ok(appsrc.upcast::<gst::Element>())
}
