//! 把签名好的 MSIX 和公钥证书嵌进安装程序。
//! 默认取 dist\VOC-<版本>.msix 和 dist\VOC.cer（先运行 deploy\windows\build-msix.ps1），
//! 也可以用环境变量 VOC_SETUP_MSIX / VOC_SETUP_CER 指定。缺文件时照常编译，安装程序运行时会提示。
use std::{env, fs, path::PathBuf};

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("..").join("..");
    let ver = env::var("CARGO_PKG_VERSION").unwrap();
    println!("cargo:rerun-if-env-changed=VOC_SETUP_MSIX");
    println!("cargo:rerun-if-env-changed=VOC_SETUP_CER");
    let pick = |var: &str, default: PathBuf| env::var(var).map(PathBuf::from).unwrap_or(default);
    let files = [
        (pick("VOC_SETUP_MSIX", root.join("dist").join(format!("VOC-{ver}.msix"))), "voc.msix"),
        (pick("VOC_SETUP_CER", root.join("dist").join("VOC.cer")), "voc.cer"),
    ];
    for (src, name) in files {
        println!("cargo:rerun-if-changed={}", src.display());
        let data = fs::read(&src).unwrap_or_default();
        if data.is_empty() {
            println!("cargo:warning=没找到 {}，这个安装程序里不会带安装包", src.display());
        }
        fs::write(out.join(name), data).unwrap();
    }
}
