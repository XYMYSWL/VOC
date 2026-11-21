//! Windows 通知（右下角弹出的消息）。只有安装版（MSIX，有应用身份）能弹；
//! 绿色版没有应用身份，系统不让弹，只靠任务栏图标闪烁和托盘未读提示。

/// 在后台线程弹一条通知（失败就算了）
pub fn toast(title: &str, body: &str) {
    if !crate::update::is_packaged() {
        return;
    }
    let (title, body) = (title.to_string(), body.to_string());
    let _ = std::thread::Builder::new().name("toast".into()).spawn(move || {
        let _ = show(&title, &body, None);
    });
}

#[cfg(windows)]
fn show(title: &str, body: &str, aumid: Option<&str>) -> windows::core::Result<()> {
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
    match aumid {
        Some(id) => ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(id))?.Show(&toast),
        None => ToastNotificationManager::CreateToastNotifier()?.Show(&toast),
    }
}

#[cfg(not(windows))]
fn show(_: &str, _: &str, _: Option<&str>) -> Result<(), ()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    /// 真弹一条通知（借 PowerShell 的应用身份，因为测试进程没有安装版的身份）。
    /// cargo test -p voc-client toast_shows -- --ignored
    #[test]
    #[ignore]
    #[cfg(windows)]
    fn toast_shows() {
        let ps = concat!("{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}", "\u{5C}", "WindowsPowerShell", "\u{5C}", "v1.0", "\u{5C}", "powershell.exe");
        super::show("VOC 测试：阿豪 在频道里 @了你", "@小明 今晚八点三角洲，来不来（这是 VOC 的测试通知）", Some(ps)).unwrap();
    }
}
