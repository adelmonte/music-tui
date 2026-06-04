use image::DynamicImage;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::FontSize;

/// Holds the terminal graphics `Picker` (detected once at startup) and builds
/// image protocols. When the terminal has no graphics support the picker is
/// `None` and the UI falls back to a text placeholder.
pub struct Art {
    picker: Option<Picker>,
    /// Protocol type detected at startup, re-applied when the picker is rebuilt
    /// for a new font size (`from_fontsize` would otherwise reset it).
    protocol_type: Option<ProtocolType>,
}

impl Art {
    /// Queries the terminal for graphics capability + font size. Must be called
    /// before the alternate screen is entered so stdio queries work.
    pub fn new() -> Self {
        let picker = Picker::from_query_stdio().ok();
        let protocol_type = picker.as_ref().map(|p| p.protocol_type());
        Self {
            picker,
            protocol_type,
        }
    }

    pub fn available(&self) -> bool {
        self.picker.is_some()
    }

    /// The terminal's current cell pixel size, used to size the art popup to the
    /// album's square aspect ratio.
    pub fn font_size(&self) -> Option<FontSize> {
        self.picker.as_ref().map(|p| p.font_size())
    }

    /// Rebuild the picker for a new terminal cell pixel size (the font size),
    /// preserving the detected protocol type. Returns true if it changed, so the
    /// caller can re-encode the current cover at the new scale.
    pub fn update_font_size(&mut self, fs: FontSize) -> bool {
        let Some(cur) = self.picker.as_ref().map(|p| p.font_size()) else {
            return false;
        };
        if cur.width == fs.width && cur.height == fs.height {
            return false;
        }
        // `from_fontsize` is deprecated in favour of `from_query_stdio`, but a
        // stdio query mid-session conflicts with the crossterm event loop. This
        // is the only API that sets an explicit font size; we re-apply the
        // protocol type detected at startup (it defaults to Halfblocks).
        #[allow(deprecated)]
        let mut p = Picker::from_fontsize(fs);
        if let Some(pt) = self.protocol_type {
            p.set_protocol_type(pt);
        }
        self.picker = Some(p);
        true
    }

    /// Wraps an already-decoded image in a resizable protocol. Cheap: the
    /// expensive decode happens off-thread; actual kitty encoding is lazy at
    /// render time inside the protocol.
    pub fn protocol_from_image(&self, img: DynamicImage) -> Option<StatefulProtocol> {
        self.picker.as_ref().map(|p| p.new_resize_protocol(img))
    }
}
