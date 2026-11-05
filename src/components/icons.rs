use gpui::{Hsla, IntoElement, SharedString, prelude::*, px, svg};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Icon {
    Appearance,
    ArrowRight,
    Bell,
    Blocks,
    Branch,
    Check,
    ChevronDown,
    ChevronLeft,
    ChevronRight,
    ChevronUp,
    Clipboard,
    Copy,
    Download,
    Enter,
    Eye,
    EyeOff,
    File,
    Folder,
    History,
    Info,
    Keyboard,
    Layouts,
    Pencil,
    Play,
    Plus,
    Refresh,
    Remote,
    Search,
    Settings,
    Sidebar,
    Smartphone,
    SplitH,
    SplitV,
    Ssh,
    Stop,
    Theme,
    Warning,
    X,
}

impl Icon {
    pub fn path(self) -> SharedString {
        let name = match self {
            Self::Appearance => "IconAppearance",
            Self::ArrowRight => "IconArrowRight",
            Self::Bell => "IconBell",
            Self::Blocks => "IconBlocks",
            Self::Branch => "IconBranch",
            Self::Check => "IconCheck",
            Self::ChevronDown => "IconChevronDown",
            Self::ChevronLeft => "IconChevronLeft",
            Self::ChevronRight => "IconChevronRight",
            Self::ChevronUp => "IconChevronUp",
            Self::Clipboard => "IconClipboard",
            Self::Copy => "IconCopy",
            Self::Download => "IconDownload",
            Self::Enter => "IconEnter",
            Self::Eye => "IconEye",
            Self::EyeOff => "IconEyeOff",
            Self::File => "IconFile",
            Self::Folder => "IconFolder",
            Self::History => "IconHistory",
            Self::Info => "IconInfo",
            Self::Keyboard => "IconKeyboard",
            Self::Layouts => "IconLayouts",
            Self::Pencil => "IconPencil",
            Self::Play => "IconPlay",
            Self::Plus => "IconPlus",
            Self::Refresh => "IconRefresh",
            Self::Remote => "IconRemote",
            Self::Search => "IconSearch",
            Self::Settings => "IconSettings",
            Self::Sidebar => "IconSidebar",
            Self::Smartphone => "IconSmartphone",
            Self::SplitH => "IconSplitH",
            Self::SplitV => "IconSplitV",
            Self::Ssh => "IconSsh",
            Self::Stop => "IconStop",
            Self::Theme => "IconTheme",
            Self::Warning => "IconWarning",
            Self::X => "IconX",
        };
        SharedString::from(format!("icons/{name}.svg"))
    }
}

pub fn icon(icon: Icon, size: f32, color: impl Into<Hsla>) -> impl IntoElement {
    svg()
        .path(icon.path())
        .flex_none()
        .size(px(size))
        .text_color(color.into())
}
