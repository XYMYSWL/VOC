//! Windows 通知（右下角弹出的消息）。只有安装版（MSIX，有应用身份）能弹；
//! 绿色版没有应用身份，系统不让弹，只靠任务栏图标闪烁和托盘未读提示。

/// 在后台线程弹一条通知（失败就算了）
pub fn toast(title: &str, body: &str) {
    if !crate::update::is_packaged() {
        return;
    }
    let (title, body) = (title.to_string(), body.to_string());
    let _ = std::thread::Builder::new().name("toast".into()).spawn(move || {
        let _ = show(&title, &body);
    });
}

#[cfg(windows)]
fn show(title: &str, body: &str) -> windows::core::Result<()> {
    use windows::core::{Interface, HSTRING};
    use windows::Data::Xml::Dom::IXmlNode;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager, ToastTemplateType};
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
    }
    let xml = ToastNotificationManager::GetTemplateContent(ToastTemplateType::ToastText02)?;
    let texts = xml.GetElementsByTagName(&HSTRING::from("text"))?;
    for (i, t) in [title, body].into_iter().enumerate() {
        let node = xml.CreateTextNode(&HSTRING::from(t))?;
        texts.Item(i as u32)?.AppendChild(&node.cast::<IXmlNode>()?)?;
    }
    let toast = ToastNotification::CreateToastNotification(&xml)?;
    ToastNotificationManager::CreateToastNotifier()?.Show(&toast)
}

#[cfg(not(windows))]
fn show(_: &str, _: &str) -> Result<(), ()> {
    Ok(())
}
