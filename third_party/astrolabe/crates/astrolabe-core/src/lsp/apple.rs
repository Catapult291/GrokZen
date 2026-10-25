//! Swift / Objective-C language-server setup that a bare checkout lacks.
//!
//! * **clangd for ObjC** needs compile flags. Without a
//!   `compile_commands.json` it parses `.m` as plain C (no `-fobjc-arc`, no
//!   header search paths, `.h` in C mode) and cross-file references come back
//!   nearly empty. When the repository has no compilation database we write a
//!   synthetic one under `.astrolabe/clangd/` and point clangd at it with
//!   `--compile-commands-dir`. A repo's own database always wins.
//! * **Off macOS** there is no Apple SDK, so `#import <Foundation/Foundation.h>`
//!   is a fatal error that stops clang before the first declaration. We
//!   generate stub framework headers (one shared prelude declaring
//!   `NSObject`, `BOOL`, `NS_ENUM`, …) so the translation unit keeps parsing
//!   and the project's own symbols get indexed.
//! * **A repo's own `compile_commands.json` with relative `directory`
//!   entries** (some generators emit `"directory": "."`). clangd joins a
//!   relative `file` onto that directory and then compares the result with
//!   the absolute path of the open document, so nothing matches and every
//!   file falls back to default flags. We write an absolutized copy under
//!   `.astrolabe/clangd/` and point clangd at it; the user's file is never
//!   modified. Applies to clangd for C, C++ and ObjC.
//! * **sourcekit-lsp** only fills its index from a build; background indexing
//!   (enabled through `initializationOptions`, see `transport.rs`) runs the
//!   SwiftPM "prepare" step itself.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::scan::{scan, ScanOptions};
use crate::types::{Language, RelPath};

use super::ServerSpec;

/// Directory (under the repo root) holding the synthetic clangd setup.
pub const CLANGD_DIR: &str = ".astrolabe/clangd";
const PRELUDE: &str = "__astrolabe_objc_prelude.h";
/// Bound the synthetic database: header search paths are one `-I` per dir.
const MAX_FILES: usize = 50_000;
const MAX_INCLUDE_DIRS: usize = 2_000;

/// Adjust a discovered server spec for clangd (C / C++ / ObjC) before it is
/// spawned.
pub fn prepare_spec(spec: &ServerSpec, root: &Path) -> ServerSpec {
    let mut spec = spec.clone();
    let objc = matches!(spec.language, Language::ObjC | Language::ObjCpp);
    let c_family = objc || matches!(spec.language, Language::C | Language::Cpp);
    if !c_family || !is_clangd(&spec.command) {
        return spec;
    }
    if objc
        && !spec
            .args
            .iter()
            .any(|a| a.starts_with("--background-index"))
    {
        spec.args.push("--background-index".into());
    }
    if spec
        .args
        .iter()
        .any(|a| a.starts_with("--compile-commands-dir"))
    {
        return spec;
    }
    if let Some(db) = own_compile_commands(root) {
        match write_absolute_compile_db(root, &db) {
            Ok(Some(dir)) => spec
                .args
                .push(format!("--compile-commands-dir={}", dir.display())),
            Ok(None) => {}
            Err(err) => tracing::warn!(
                error = %err,
                db = %db.display(),
                "could not write absolutized compile_commands.json; clangd uses the original"
            ),
        }
        return spec;
    }
    if !objc || has_own_compile_db(root) {
        return spec;
    }
    match write_objc_compile_db(root, apple_sdk().as_deref()) {
        Ok(dir) => spec
            .args
            .push(format!("--compile-commands-dir={}", dir.display())),
        Err(err) => {
            tracing::warn!(error = %err, "could not write synthetic ObjC compile_commands.json")
        }
    }
    spec
}

/// A source file to open right after clangd starts for ObjC.
///
/// clangd loads the compilation database, and so starts its background
/// index, only when the first document is opened. Until then it reports no
/// progress, the pool decides it is idle, and the first references query is
/// answered from an index that has not started yet. Opening one `.m` from the
/// synthetic database at spawn makes indexing (and its `$/progress`) begin
/// immediately, so the readiness wait sees it.
pub fn warm_up_file(spec: &ServerSpec, root: &Path) -> Option<RelPath> {
    if !matches!(spec.language, Language::ObjC | Language::ObjCpp) || !is_clangd(&spec.command) {
        return None;
    }
    let db = std::fs::read_to_string(root.join(CLANGD_DIR).join("compile_commands.json")).ok()?;
    let entries: Vec<Value> = serde_json::from_str(&db).ok()?;
    entries.iter().find_map(|e| {
        let file = Path::new(e.get("file")?.as_str()?);
        if !file.extension().is_some_and(|x| x == "m" || x == "mm") {
            return None;
        }
        let rel = file.strip_prefix(root).ok()?;
        Some(RelPath::new(rel.to_string_lossy().replace('\\', "/")))
    })
}

fn is_clangd(command: &Path) -> bool {
    command
        .file_stem()
        .is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case("clangd"))
}

/// The repository ships its own flags (CMake export, `bear`, xcpretty, …).
pub fn has_own_compile_db(root: &Path) -> bool {
    [
        "compile_commands.json",
        "build/compile_commands.json",
        "compile_flags.txt",
    ]
    .iter()
    .any(|p| root.join(p).is_file())
}

/// The repository's own `compile_commands.json`, at the root or in `build/`
/// (the two places clangd itself looks from the root).
fn own_compile_commands(root: &Path) -> Option<PathBuf> {
    ["compile_commands.json", "build/compile_commands.json"]
        .iter()
        .map(|p| root.join(p))
        .find(|p| p.is_file())
}

/// When `db` has entries whose `directory` is relative, write a copy with
/// those directories made absolute to `<root>/.astrolabe/clangd/` and return
/// that directory. `Ok(None)`: every entry is already absolute (or the file
/// is not a JSON array), so clangd should read the original.
///
/// A relative `directory` is resolved against the directory holding the
/// database, which is where the generator ran for `build/` exports and the
/// root for a root-level file. Other fields are left untouched: `file`,
/// `arguments` and `output` are relative to `directory` by definition, and
/// become correct once it is absolute.
pub fn write_absolute_compile_db(root: &Path, db: &Path) -> std::io::Result<Option<PathBuf>> {
    let text = std::fs::read_to_string(db)?;
    let Ok(Value::Array(mut entries)) = serde_json::from_str::<Value>(&text) else {
        return Ok(None);
    };
    let base = db.parent().unwrap_or(root);
    let base = std::fs::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());
    let mut rewritten = 0usize;
    for entry in &mut entries {
        let Some(dir) = entry.get("directory").and_then(Value::as_str) else {
            continue;
        };
        if Path::new(dir).is_absolute() {
            continue;
        }
        let abs = normalize(&base.join(dir));
        entry["directory"] = Value::String(abs.to_string_lossy().into_owned());
        rewritten += 1;
    }
    if rewritten == 0 {
        return Ok(None);
    }
    let out = root.join(CLANGD_DIR);
    std::fs::create_dir_all(&out)?;
    let body =
        serde_json::to_string_pretty(&Value::Array(entries)).map_err(std::io::Error::other)?;
    write_if_changed(&out.join("compile_commands.json"), &body)?;
    tracing::info!(
        db = %db.display(),
        rewritten,
        "compile_commands.json had relative directories; clangd reads an absolutized copy"
    );
    Ok(Some(out))
}

/// Drop `.` and resolve `..` lexically (the directory may not exist yet).
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `xcrun --show-sdk-path` on macOS; `None` elsewhere or without Xcode.
fn apple_sdk() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = std::process::Command::new("xcrun")
        .arg("--show-sdk-path")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let path = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !path.is_empty()).then_some(path)
}

/// Write `<root>/.astrolabe/clangd/compile_commands.json` (and, without an
/// SDK, stub framework headers). Returns the directory to hand to clangd.
pub fn write_objc_compile_db(root: &Path, sdk: Option<&str>) -> std::io::Result<PathBuf> {
    let dir = root.join(CLANGD_DIR);
    std::fs::create_dir_all(&dir)?;
    let mut files: Vec<RelPath> = scan(root, &ScanOptions::default())
        .files
        .into_iter()
        .filter(|f| {
            f.extension()
                .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "m" | "mm" | "h"))
        })
        .collect();
    files.sort();
    files.truncate(MAX_FILES);

    let mut include_dirs = BTreeSet::new();
    let mut angle_imports = BTreeSet::new();
    let mut entries = Vec::new();
    for file in &files {
        let abs = root.join(file.as_str());
        if let Some(parent) = abs.parent() {
            if include_dirs.len() < MAX_INCLUDE_DIRS {
                include_dirs.insert(parent.to_path_buf());
            }
        }
        let source = std::fs::read_to_string(&abs).unwrap_or_default();
        if sdk.is_none() {
            collect_angle_imports(&source, &mut angle_imports);
        }
        let ext = file.extension().unwrap_or_default().to_ascii_lowercase();
        let lang = match ext.as_str() {
            "mm" => "objective-c++",
            "m" => "objective-c",
            // Only ObjC headers (same sniff the indexer uses) are compiled as
            // ObjC; C/C++ headers in a mixed repo keep clangd's own inference.
            _ => match crate::parse::objc_header_language(&source) {
                Some(Language::ObjCpp) => "objective-c++",
                Some(_) => "objective-c",
                None => continue,
            },
        };
        entries.push((abs, lang));
    }

    let mut base: Vec<String> = vec![
        "-fobjc-arc".into(),
        // Blocks are on by default only for Apple targets; ObjC APIs are
        // full of block-typed parameters and properties.
        "-fblocks".into(),
        "-ferror-limit=0".into(),
        "-Wno-everything".into(),
    ];
    match sdk {
        Some(sdk) => {
            base.push("-isysroot".into());
            base.push(sdk.to_string());
            base.push("-fmodules".into());
        }
        None => {
            let shim = dir.join("shim");
            write_shim(root, &shim, &angle_imports)?;
            // The GNU runtime rejects ARC; use the Apple runtime model.
            base.push("-fobjc-runtime=macosx".into());
            base.push(format!("-I{}", shim.display()));
            base.push("-include".into());
            base.push(shim.join(PRELUDE).display().to_string());
            for def in [
                "TARGET_OS_IOS=1",
                "TARGET_OS_IPHONE=1",
                "TARGET_OS_MAC=1",
                "TARGET_OS_OSX=0",
                "TARGET_OS_TV=0",
                "TARGET_OS_WATCH=0",
                "TARGET_OS_VISION=0",
                "TARGET_OS_MACCATALYST=0",
                "TARGET_OS_SIMULATOR=0",
            ] {
                base.push(format!("-D{def}"));
            }
        }
    }
    base.extend(include_dirs.iter().map(|d| format!("-I{}", d.display())));

    let commands: Vec<Value> = entries
        .iter()
        .map(|(abs, lang)| {
            let mut args = vec!["clang".to_string(), "-x".into(), (*lang).into()];
            args.extend(base.iter().cloned());
            args.push("-c".into());
            args.push(abs.display().to_string());
            json!({
                "directory": root.display().to_string(),
                "file": abs.display().to_string(),
                "arguments": args,
            })
        })
        .collect();
    let body = serde_json::to_string(&commands).map_err(std::io::Error::other)?;
    write_if_changed(&dir.join("compile_commands.json"), &body)?;
    Ok(dir)
}

/// Apple SDK headers imported without a framework prefix. Off macOS they do
/// not exist, and a missing `#import <TargetConditionals.h>` is a *fatal*
/// error: clang stops the translation unit there and nothing after it (nor
/// any file that includes the header) is indexed. Plain C/POSIX headers are
/// deliberately absent so real system headers are never shadowed.
const APPLE_BARE_HEADERS: &[&str] = &[
    "TargetConditionals.h",
    "Availability.h",
    "AvailabilityMacros.h",
    "AvailabilityInternal.h",
    "AvailabilityVersions.h",
    "MacTypes.h",
    "AssertMacros.h",
];

/// `<Framework/Header.h>` imports that the repository does not provide.
fn collect_angle_imports(source: &str, out: &mut BTreeSet<String>) {
    for line in source.lines() {
        let line = line.trim_start();
        let rest = ["#import", "#include", "# import", "# include"]
            .iter()
            .find_map(|p| line.strip_prefix(p));
        let Some(rest) = rest else { continue };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('<') else {
            continue;
        };
        let Some(end) = rest.find('>') else { continue };
        let header = &rest[..end];
        let framework = header.contains('/') && !header.contains("..") && !header.starts_with('/');
        if framework || APPLE_BARE_HEADERS.contains(&header) {
            out.insert(header.to_string());
        }
    }
}

fn write_shim(root: &Path, shim: &Path, imports: &BTreeSet<String>) -> std::io::Result<()> {
    std::fs::create_dir_all(shim)?;
    write_if_changed(&shim.join(PRELUDE), OBJC_PRELUDE)?;
    let mut headers = BTreeMap::new();
    if !imports.is_empty() {
        index_headers(root, 8, &mut headers);
    }
    for header in imports {
        let path = shim.join(header);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Framework-style imports of the project's own headers
        // (`<SDWebImage/SDImageCache.h>` while the file lives in
        // `SDWebImage/Core/`) cannot resolve through plain `-I` dirs, and the
        // miss is fatal. Forward them to the real file; stub only what the
        // repository cannot provide.
        let body = match find_own_header(root, &headers, header) {
            Some(real) => format!("#import \"{}\"\n", real.display()),
            None => format!("#include \"{PRELUDE}\"\n"),
        };
        write_if_changed(&path, &body)?;
    }
    Ok(())
}

/// The repository file a `<Framework/Header.h>` import means, if any:
/// a file named `Header.h`, preferring one under a directory named
/// `Framework`.
fn find_own_header(
    root: &Path,
    headers: &BTreeMap<String, Vec<PathBuf>>,
    header: &str,
) -> Option<PathBuf> {
    let mut parts = header.rsplit('/');
    let found = headers.get(parts.next()?)?;
    let framework = parts.next_back();
    framework
        .and_then(|fw| {
            found.iter().find(|p| {
                p.strip_prefix(root)
                    .is_ok_and(|r| r.components().any(|c| c.as_os_str() == fw))
            })
        })
        .or_else(|| found.first())
        .cloned()
}

/// File name -> paths of every `.h` in the repository (bounded depth).
fn index_headers(dir: &Path, depth: u32, out: &mut BTreeMap<String, Vec<PathBuf>>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || matches!(name.as_str(), "Pods" | "build" | "node_modules") {
            continue;
        }
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_file() && name.ends_with(".h") {
            out.entry(name).or_default().push(entry.path());
        } else if ft.is_dir() && depth > 0 {
            index_headers(&entry.path(), depth - 1, out);
        }
    }
}

fn write_if_changed(path: &Path, body: &str) -> std::io::Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|old| old == body) {
        return Ok(());
    }
    std::fs::write(path, body)
}

/// Just enough of Foundation for clang to keep parsing project code. Types
/// it does not know become ordinary (non-fatal) errors; the project's own
/// classes, protocols and methods still get indexed.
const OBJC_PRELUDE: &str = r#"#pragma once
// Generated by Astrolabe: minimal Apple SDK stand-in for indexing off macOS.
#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>
typedef signed char BOOL;
typedef long NSInteger;
typedef unsigned long NSUInteger;
typedef double CGFloat;
typedef double NSTimeInterval;
typedef struct CGPoint { CGFloat x, y; } CGPoint;
typedef struct CGSize { CGFloat width, height; } CGSize;
typedef struct CGRect { CGPoint origin; CGSize size; } CGRect;
typedef struct _NSRange { NSUInteger location, length; } NSRange;
typedef double CFTimeInterval;
typedef long dispatch_once_t;
typedef unsigned long long dispatch_time_t;
typedef struct _NSZone NSZone;
typedef struct objc_method *Method;
typedef id (*IMP)(id, SEL, ...);
#define __unused __attribute__((unused))
#define IBOutlet
#define IBAction void
#define IBInspectable
#define IB_DESIGNABLE
typedef NSInteger UIControlState;
typedef NSInteger UIControlEvents;
typedef NSInteger UIViewContentMode;
typedef NSInteger UITableViewCellStyle;
typedef NSInteger UIImageOrientation;
typedef NSInteger UIUserInterfaceStyle;
typedef NSInteger NSURLRequestCachePolicy;
typedef NSInteger NSURLRequestNetworkServiceType;
typedef NSInteger NSURLSessionAuthChallengeDisposition;
typedef NSInteger NSURLSessionResponseDisposition;
typedef NSInteger NSURLSessionTaskState;
typedef NSInteger NSURLSessionMultipathServiceType;
typedef NSInteger NSStreamStatus;
typedef NSInteger NSStreamEvent;
typedef NSInteger NSQualityOfService;
typedef NSInteger NSOperationQueuePriority;
typedef NSInteger NSDataReadingOptions;
typedef NSInteger NSDataWritingOptions;
typedef NSInteger NSStringCompareOptions;
typedef NSInteger NSEnumerationOptions;
typedef NSInteger NSKeyValueObservingOptions;
typedef NSInteger NSSearchPathDirectory;
typedef NSInteger NSSearchPathDomainMask;
typedef NSInteger SCNetworkReachabilityFlags;
typedef NSInteger CFStringEncoding;
typedef NSInteger CFIndex;
typedef NSInteger CFOptionFlags;
typedef NSInteger OSStatus;
typedef struct __astrolabe_CFTypeRef *CFTypeRef;
typedef struct __astrolabe_CFStringRef *CFStringRef;
typedef struct __astrolabe_CFMutableStringRef *CFMutableStringRef;
typedef struct __astrolabe_CFDataRef *CFDataRef;
typedef struct __astrolabe_CFMutableDataRef *CFMutableDataRef;
typedef struct __astrolabe_CFArrayRef *CFArrayRef;
typedef struct __astrolabe_CFMutableArrayRef *CFMutableArrayRef;
typedef struct __astrolabe_CFDictionaryRef *CFDictionaryRef;
typedef struct __astrolabe_CFMutableDictionaryRef *CFMutableDictionaryRef;
typedef struct __astrolabe_CFURLRef *CFURLRef;
typedef struct __astrolabe_CFAllocatorRef *CFAllocatorRef;
typedef struct __astrolabe_CFRunLoopRef *CFRunLoopRef;
typedef struct __astrolabe_CFReadStreamRef *CFReadStreamRef;
typedef struct __astrolabe_CFWriteStreamRef *CFWriteStreamRef;
typedef struct __astrolabe_CGImageRef *CGImageRef;
typedef struct __astrolabe_CGContextRef *CGContextRef;
typedef struct __astrolabe_CGColorSpaceRef *CGColorSpaceRef;
typedef struct __astrolabe_CGColorRef *CGColorRef;
typedef struct __astrolabe_CGDataProviderRef *CGDataProviderRef;
typedef struct __astrolabe_CGImageSourceRef *CGImageSourceRef;
typedef struct __astrolabe_CGImageDestinationRef *CGImageDestinationRef;
typedef struct __astrolabe_SecKeyRef *SecKeyRef;
typedef struct __astrolabe_SCNetworkReachabilityRef *SCNetworkReachabilityRef;
typedef struct __astrolabe_dispatch_queue_t *dispatch_queue_t;
typedef struct __astrolabe_dispatch_group_t *dispatch_group_t;
typedef struct __astrolabe_dispatch_semaphore_t *dispatch_semaphore_t;
typedef struct __astrolabe_dispatch_source_t *dispatch_source_t;
typedef unsigned char UInt8;
typedef unsigned short UInt16;
typedef unsigned int UInt32;
typedef unsigned long long UInt64;
typedef signed char SInt8;
typedef short SInt16;
typedef int SInt32;
typedef long long SInt64;
typedef NSUInteger NSJSONReadingOptions;
typedef NSUInteger NSJSONWritingOptions;
typedef NSUInteger NSPropertyListReadOptions;
typedef NSUInteger NSPropertyListWriteOptions;
typedef NSUInteger NSPropertyListFormat;
typedef NSInteger NSComparisonResult;
typedef NSUInteger NSStringEncoding;
@protocol NSObject
@end
@protocol NSCopying
@end
@protocol NSMutableCopying
@end
@protocol NSCoding
@end
@protocol NSSecureCoding <NSCoding>
@end
@protocol NSFastEnumeration
@end
__attribute__((objc_root_class))
@interface NSObject <NSObject>
+ (instancetype)alloc;
+ (Class)class;
- (Class)class;
+ (BOOL)isKindOfClass:(Class)cls;
- (BOOL)isKindOfClass:(Class)cls;
- (BOOL)respondsToSelector:(SEL)sel;
- (BOOL)isEqual:(id)object;
- (NSUInteger)hash;
+ (instancetype)new;
- (instancetype)init;
@end
// Common SDK classes: real (empty) interfaces, not forward @class, so class
// messages and lightweight generics (`NSArray<NSString *>`) keep parsing.
@interface NSString : NSObject
@end
@interface NSMutableString : NSObject
@end
@interface NSAttributedString : NSObject
@end
@interface NSMutableAttributedString : NSObject
@end
@interface NSError : NSObject
@end
@interface NSURL : NSObject
@end
@interface NSURLComponents : NSObject
@end
@interface NSURLQueryItem : NSObject
@end
@interface NSData : NSObject
@end
@interface NSMutableData : NSObject
@end
@interface NSNumber : NSObject
@end
@interface NSValue : NSObject
@end
@interface NSDecimalNumber : NSObject
@end
@interface NSDate : NSObject
@end
@interface NSDateFormatter : NSObject
@end
@interface NSNumberFormatter : NSObject
@end
@interface NSCalendar : NSObject
@end
@interface NSLocale : NSObject
@end
@interface NSTimeZone : NSObject
@end
@interface NSCoder : NSObject
@end
@interface NSKeyedArchiver : NSObject
@end
@interface NSKeyedUnarchiver : NSObject
@end
@interface NSBundle : NSObject
@end
@interface NSOperation : NSObject
@end
@interface NSBlockOperation : NSObject
@end
@interface NSOperationQueue : NSObject
@end
@interface NSThread : NSObject
@end
@interface NSRunLoop : NSObject
@end
@interface NSTimer : NSObject
@end
@interface NSLock : NSObject
@end
@interface NSRecursiveLock : NSObject
@end
@interface NSCondition : NSObject
@end
@interface NSNotification : NSObject
@end
@interface NSNotificationCenter : NSObject
@end
@interface NSUserDefaults : NSObject
@end
@interface NSFileManager : NSObject
@end
@interface NSFileHandle : NSObject
@end
@interface NSInputStream : NSObject
@end
@interface NSOutputStream : NSObject
@end
@interface NSStream : NSObject
@end
@interface NSUUID : NSObject
@end
@interface NSIndexSet : NSObject
@end
@interface NSMutableIndexSet : NSObject
@end
@interface NSIndexPath : NSObject
@end
@interface NSProgress : NSObject
@end
@interface NSPredicate : NSObject
@end
@interface NSRegularExpression : NSObject
@end
@interface NSScanner : NSObject
@end
@interface NSCharacterSet : NSObject
@end
@interface NSHTTPCookie : NSObject
@end
@interface NSHTTPCookieStorage : NSObject
@end
@interface NSURLRequest : NSObject
@end
@interface NSMutableURLRequest : NSObject
@end
@interface NSURLResponse : NSObject
@end
@interface NSHTTPURLResponse : NSObject
@end
@interface NSCachedURLResponse : NSObject
@end
@interface NSURLCache : NSObject
@end
@interface NSURLCredential : NSObject
@end
@interface NSURLProtectionSpace : NSObject
@end
@interface NSURLAuthenticationChallenge : NSObject
@end
@interface NSURLSession : NSObject
@end
@interface NSURLSessionConfiguration : NSObject
@end
@interface NSURLSessionTask : NSObject
@end
@interface NSURLSessionDataTask : NSObject
@end
@interface NSURLSessionUploadTask : NSObject
@end
@interface NSURLSessionDownloadTask : NSObject
@end
@interface NSURLSessionStreamTask : NSObject
@end
@interface NSURLSessionTaskMetrics : NSObject
@end
@interface NSJSONSerialization : NSObject
@end
@interface NSPropertyListSerialization : NSObject
@end
@interface NSProcessInfo : NSObject
@end
@interface NSException : NSObject
@end
@interface NSInvocation : NSObject
@end
@interface NSMethodSignature : NSObject
@end
@interface NSNull : NSObject
@end
@interface NSProxy : NSObject
@end
@interface NSSecureUnarchiveFromDataTransformer : NSObject
@end
@interface NSValueTransformer : NSObject
@end
@interface UIImage : NSObject
@end
@interface UIImageView : NSObject
@end
@interface UIView : NSObject
@end
@interface UIViewController : NSObject
@end
@interface UIColor : NSObject
@end
@interface UIFont : NSObject
@end
@interface UIScreen : NSObject
@end
@interface UIApplication : NSObject
@end
@interface UIWindow : NSObject
@end
@interface UIButton : NSObject
@end
@interface UILabel : NSObject
@end
@interface UIControl : NSObject
@end
@interface UIScrollView : NSObject
@end
@interface UITableView : NSObject
@end
@interface UITableViewCell : NSObject
@end
@interface UICollectionView : NSObject
@end
@interface UICollectionViewCell : NSObject
@end
@interface UIGraphicsImageRenderer : NSObject
@end
@interface UIGraphicsImageRendererFormat : NSObject
@end
@interface UIActivityIndicatorView : NSObject
@end
@interface UIProgressView : NSObject
@end
@interface UIRefreshControl : NSObject
@end
@interface UIWebView : NSObject
@end
@interface UIEvent : NSObject
@end
@interface UITouch : NSObject
@end
@interface UIGestureRecognizer : NSObject
@end
@interface UIBezierPath : NSObject
@end
@interface NSImage : NSObject
@end
@interface NSImageView : NSObject
@end
@interface NSView : NSObject
@end
@interface NSButton : NSObject
@end
@interface NSColor : NSObject
@end
@interface NSScreen : NSObject
@end
@interface NSWindow : NSObject
@end
@interface NSGraphicsContext : NSObject
@end
@interface NSBitmapImageRep : NSObject
@end
@interface NSImageRep : NSObject
@end
@interface CALayer : NSObject
@end
@interface CAAnimation : NSObject
@end
@interface CABasicAnimation : NSObject
@end
@interface CATransition : NSObject
@end
@interface CADisplayLink : NSObject
@end
@interface AVAsset : NSObject
@end
@interface AVPlayer : NSObject
@end
@interface PHAsset : NSObject
@end
@interface MKAnnotationView : NSObject
@end
@interface WKWebView : NSObject
@end
@interface UINavigationController : NSObject
@end
@interface UITableViewController : NSObject
@end
@interface UITabBarController : NSObject
@end
@interface NSTableView : NSObject
@end
@interface NSArrayController : NSObject
@end
@interface NSApplication : NSObject
@end
@interface WKNavigation : NSObject
@end
@interface WKNavigationAction : NSObject
@end
@interface WKNavigationResponse : NSObject
@end
@interface XCTKVOExpectation : NSObject
@end
@interface XCTNSPredicateExpectation : NSObject
@end
typedef NSString *NSErrorDomain;
typedef NSString *NSNotificationName;
typedef NSString *NSRunLoopMode;
@interface NSArray<ObjectType> : NSObject
@end
@interface NSMutableArray<ObjectType> : NSObject
@end
@interface NSSet<ObjectType> : NSObject
@end
@interface NSMutableSet<ObjectType> : NSObject
@end
@interface NSOrderedSet<ObjectType> : NSObject
@end
@interface NSMutableOrderedSet<ObjectType> : NSObject
@end
@interface NSHashTable<ObjectType> : NSObject
@end
@interface NSPointerArray<ObjectType> : NSObject
@end
@interface NSEnumerator<ObjectType> : NSObject
@end
@interface NSCountedSet<ObjectType> : NSObject
@end
@interface NSDictionary<KeyType, ObjectType> : NSObject
@end
@interface NSMutableDictionary<KeyType, ObjectType> : NSObject
@end
@interface NSMapTable<KeyType, ObjectType> : NSObject
@end
@interface NSCache<KeyType, ObjectType> : NSObject
@end
#define NS_ASSUME_NONNULL_BEGIN _Pragma("clang assume_nonnull begin")
#define NS_ASSUME_NONNULL_END _Pragma("clang assume_nonnull end")
#define NS_DESIGNATED_INITIALIZER
#define NS_UNAVAILABLE
#define NS_REQUIRES_SUPER
#define NS_SWIFT_NAME(...)
#define NS_SWIFT_NOTHROW
#define NS_SWIFT_UI_ACTOR
#define NS_SWIFT_SENDABLE
#define NS_NOESCAPE __attribute__((noescape))
#define NS_FORMAT_FUNCTION(...)
#define NS_RETURNS_RETAINED
#define NS_RETURNS_NOT_RETAINED
#define NS_REQUIRES_NIL_TERMINATION
#define NS_EXTENSION_UNAVAILABLE_IOS(...)
#define NS_EXTENSION_UNAVAILABLE(...)
#define API_AVAILABLE_BEGIN(...)
#define API_AVAILABLE_END
#define NS_HEADER_AUDIT_BEGIN(...)
#define NS_HEADER_AUDIT_END(...)
#define CF_RETURNS_RETAINED
#define CF_RETURNS_NOT_RETAINED
#define NS_SWIFT_UNAVAILABLE(...)
#define NS_REFINED_FOR_SWIFT
#define NS_EXTENSIBLE_STRING_ENUM
#define NS_TYPED_ENUM
#define NS_TYPED_EXTENSIBLE_ENUM
#define NS_ENUM(_type, _name) enum _name : _type _name; enum _name : _type
#define NS_OPTIONS(_type, _name) enum _name : _type _name; enum _name : _type
#define NS_CLOSED_ENUM(_type, _name) enum _name : _type _name; enum _name : _type
#define NS_ERROR_ENUM(_dom, _name) enum _name : NSInteger
#define FOUNDATION_EXPORT extern
#define FOUNDATION_EXTERN extern
#define UIKIT_EXTERN extern
#define NS_INLINE static inline
#define API_AVAILABLE(...)
#define API_UNAVAILABLE(...)
#define API_DEPRECATED(...)
#define API_DEPRECATED_WITH_REPLACEMENT(...)
#define NS_AVAILABLE(...)
#define NS_AVAILABLE_IOS(...)
#define NS_DEPRECATED(...)
#define NS_DEPRECATED_IOS(...)
#define __IOS_AVAILABLE(...)
#define __TVOS_PROHIBITED
#define __WATCHOS_PROHIBITED
// XCTest assertions: keep the arguments as expressions so references inside
// test assertions (`XCTAssertEqual(x, obj.prop)`) are still indexed.
#define XCTAssert(...) ((void)0, ##__VA_ARGS__)
#define XCTAssertTrue(...) ((void)0, ##__VA_ARGS__)
#define XCTAssertFalse(...) ((void)0, ##__VA_ARGS__)
#define XCTAssertNil(...) ((void)0, ##__VA_ARGS__)
#define XCTAssertNotNil(...) ((void)0, ##__VA_ARGS__)
#define XCTAssertEqual(...) ((void)0, ##__VA_ARGS__)
#define XCTAssertNotEqual(...) ((void)0, ##__VA_ARGS__)
#define XCTAssertEqualObjects(...) ((void)0, ##__VA_ARGS__)
#define XCTAssertNotEqualObjects(...) ((void)0, ##__VA_ARGS__)
#define XCTAssertEqualWithAccuracy(...) ((void)0, ##__VA_ARGS__)
#define XCTAssertNotEqualWithAccuracy(...) ((void)0, ##__VA_ARGS__)
#define XCTAssertGreaterThan(...) ((void)0, ##__VA_ARGS__)
#define XCTAssertGreaterThanOrEqual(...) ((void)0, ##__VA_ARGS__)
#define XCTAssertLessThan(...) ((void)0, ##__VA_ARGS__)
#define XCTAssertLessThanOrEqual(...) ((void)0, ##__VA_ARGS__)
#define XCTAssertIdentical(...) ((void)0, ##__VA_ARGS__)
#define XCTAssertNotIdentical(...) ((void)0, ##__VA_ARGS__)
#define XCTAssertThrows(...) ((void)0, ##__VA_ARGS__)
#define XCTAssertNoThrow(...) ((void)0, ##__VA_ARGS__)
#define XCTAssertThrowsSpecific(...) ((void)0, ##__VA_ARGS__)
#define XCTAssertThrowsSpecificNamed(...) ((void)0, ##__VA_ARGS__)
#define XCTAssertNoThrowSpecific(...) ((void)0, ##__VA_ARGS__)
#define XCTFail(...) ((void)0, ##__VA_ARGS__)
typedef struct __SecTrust *SecTrustRef;
typedef struct __SecCertificate *SecCertificateRef;
typedef struct __SecPolicy *SecPolicyRef;
typedef struct __SecIdentity *SecIdentityRef;
typedef void (^XCWaitCompletionHandler)(NSError *error);
@interface XCTestExpectation : NSObject
- (void)fulfill;
@end
@interface XCTestCase : NSObject
- (XCTestExpectation *)expectationWithDescription:(NSString *)description;
- (void)waitForExpectationsWithTimeout:(NSTimeInterval)timeout handler:(XCWaitCompletionHandler)handler;
- (void)setUp;
- (void)tearDown;
@end
#define YES ((BOOL)1)
#define NO ((BOOL)0)
#ifndef nil
#define nil ((id)0)
#endif
#ifndef NULL
#define NULL ((void *)0)
#endif
"#;

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tmp(name: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!(
            "astrolabe-apple-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }

    fn objc_spec(cmd: &str) -> ServerSpec {
        ServerSpec {
            language: Language::ObjC,
            command: PathBuf::from(cmd),
            args: Vec::new(),
            install_hint: String::new(),
        }
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn clangd_for_objc_gets_synthetic_compile_db() {
        let t = tmp("cdb");
        write(
            &t.0,
            "Lib/A.h",
            "#import <Foundation/Foundation.h>\n#import <Lib/B.h>\n@interface A : NSObject\n@end\n",
        );
        write(
            &t.0,
            "Lib/B.h",
            "#import <Foundation/Foundation.h>\n@interface B : NSObject\n@end\n",
        );
        write(
            &t.0,
            "Lib/A.m",
            "#import \"A.h\"\n@implementation A\n@end\n",
        );
        write(&t.0, "Lib/c.h", "int plain(void);\n");
        let spec = prepare_spec(&objc_spec("/usr/bin/clangd"), &t.0);
        assert!(spec.args.contains(&"--background-index".to_string()));
        let dir = spec
            .args
            .iter()
            .find_map(|a| a.strip_prefix("--compile-commands-dir="))
            .expect("compile-commands-dir");
        let db: Vec<Value> = serde_json::from_str(
            &std::fs::read_to_string(Path::new(dir).join("compile_commands.json")).unwrap(),
        )
        .unwrap();
        let files: Vec<String> = db
            .iter()
            .map(|e| e["file"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(files.len(), 3, "{files:?}");
        assert!(
            !files.iter().any(|f| f.ends_with("c.h")),
            "plain C header stays out"
        );
        let args: Vec<String> = serde_json::from_value(db[0]["arguments"].clone()).unwrap();
        assert!(args.contains(&"objective-c".to_string()));
        assert!(args.contains(&"-fobjc-arc".to_string()));
        assert!(args
            .iter()
            .any(|a| a.starts_with("-I") && a.ends_with("Lib")));
        assert_eq!(
            warm_up_file(&spec, &t.0).map(|r| r.as_str().to_string()),
            Some("Lib/A.m".to_string())
        );
        if apple_sdk().is_none() {
            let shim = Path::new(dir).join("shim");
            assert!(shim.join("Foundation/Foundation.h").is_file());
            // The project's own framework-style header forwards to the
            // real file instead of being stubbed.
            let fwd = std::fs::read_to_string(shim.join("Lib/B.h")).unwrap();
            assert!(fwd.contains("#import") && fwd.contains("B.h") && !fwd.contains(PRELUDE));
            assert!(args.contains(&"-fobjc-runtime=macosx".to_string()));
        }
    }

    fn c_spec(language: Language, cmd: &str) -> ServerSpec {
        ServerSpec {
            language,
            command: PathBuf::from(cmd),
            args: Vec::new(),
            install_hint: String::new(),
        }
    }

    fn db_dir(spec: &ServerSpec) -> Option<&str> {
        spec.args
            .iter()
            .find_map(|a| a.strip_prefix("--compile-commands-dir="))
    }

    /// S4: relative `directory` entries in the repo's own database are made
    /// absolute in a copy; the original is untouched.
    #[test]
    fn relative_compile_db_directories_are_absolutized_in_a_copy() {
        let t = tmp("reldb");
        let original = r#"[
  {"directory": ".", "file": "src/a.c", "arguments": ["cc", "-Iinclude", "-c", "src/a.c"]},
  {"directory": "sub/../lib", "file": "b.c", "command": "cc -c b.c"},
  {"directory": "/abs/already", "file": "c.c", "command": "cc -c c.c"}
]"#;
        write(&t.0, "compile_commands.json", original);
        let spec = prepare_spec(&c_spec(Language::C, "/usr/bin/clangd"), &t.0);
        let dir = db_dir(&spec)
            .expect("points clangd at the copy")
            .to_string();
        assert_eq!(Path::new(&dir), t.0.join(CLANGD_DIR));
        assert!(
            !spec
                .args
                .iter()
                .any(|a| a.starts_with("--background-index")),
            "C/C++ args are otherwise unchanged"
        );
        let copy: Vec<Value> = serde_json::from_str(
            &std::fs::read_to_string(Path::new(&dir).join("compile_commands.json")).unwrap(),
        )
        .unwrap();
        let root = std::fs::canonicalize(&t.0).unwrap();
        assert_eq!(copy[0]["directory"], json!(root.to_string_lossy()));
        assert_eq!(
            copy[1]["directory"],
            json!(root.join("lib").to_string_lossy())
        );
        assert_eq!(copy[2]["directory"], json!("/abs/already"));
        assert_eq!(
            copy[0]["arguments"][1],
            json!("-Iinclude"),
            "flags untouched"
        );
        assert_eq!(copy[0]["file"], json!("src/a.c"));
        assert_eq!(
            std::fs::read_to_string(t.0.join("compile_commands.json")).unwrap(),
            original
        );

        // C++ and ObjC clangd get the same treatment; other servers do not.
        let cpp = prepare_spec(&c_spec(Language::Cpp, "clangd"), &t.0);
        assert_eq!(db_dir(&cpp), Some(dir.as_str()));
        let objc = prepare_spec(&objc_spec("clangd"), &t.0);
        assert_eq!(db_dir(&objc), Some(dir.as_str()));
        let ccls = prepare_spec(&c_spec(Language::C, "ccls"), &t.0);
        assert!(ccls.args.is_empty());
        // An explicit user choice wins.
        let mut pinned = c_spec(Language::C, "clangd");
        pinned.args.push("--compile-commands-dir=/elsewhere".into());
        assert_eq!(prepare_spec(&pinned, &t.0).args, pinned.args);
    }

    /// S4: `build/compile_commands.json` resolves `.` against `build/`, and a
    /// database that is already absolute is passed through as is.
    #[test]
    fn build_dir_db_resolves_against_its_own_directory() {
        let t = tmp("builddb");
        write(
            &t.0,
            "build/compile_commands.json",
            r#"[{"directory": ".", "file": "../src/a.c", "command": "cc -c ../src/a.c"}]"#,
        );
        let spec = prepare_spec(&c_spec(Language::Cpp, "clangd"), &t.0);
        let dir = db_dir(&spec).expect("copy");
        let copy: Vec<Value> = serde_json::from_str(
            &std::fs::read_to_string(Path::new(dir).join("compile_commands.json")).unwrap(),
        )
        .unwrap();
        let build = std::fs::canonicalize(t.0.join("build")).unwrap();
        assert_eq!(copy[0]["directory"], json!(build.to_string_lossy()));

        let t2 = tmp("absdb");
        write(
            &t2.0,
            "compile_commands.json",
            r#"[{"directory": "/abs", "file": "a.c", "command": "cc -c a.c"}]"#,
        );
        let spec = prepare_spec(&c_spec(Language::C, "clangd"), &t2.0);
        assert!(spec.args.is_empty(), "{:?}", spec.args);
        assert!(!t2.0.join(CLANGD_DIR).exists());
    }

    #[test]
    fn own_compile_db_and_other_servers_are_left_alone() {
        let t = tmp("own");
        write(&t.0, "A.m", "@implementation A\n@end\n");
        write(&t.0, "compile_commands.json", "[]");
        let spec = prepare_spec(&objc_spec("clangd"), &t.0);
        assert_eq!(spec.args, vec!["--background-index".to_string()]);
        assert!(!t.0.join(CLANGD_DIR).exists());

        let sk = prepare_spec(&objc_spec("sourcekit-lsp"), &t.0);
        assert!(sk.args.is_empty());
        let mut c = objc_spec("clangd");
        c.language = Language::C;
        assert!(prepare_spec(&c, &t.0).args.is_empty());
    }

    #[test]
    fn angle_imports_skip_quoted_and_system_c_headers() {
        let mut out = BTreeSet::new();
        collect_angle_imports(
            "#import <UIKit/UIKit.h>\n#include <stdio.h>\n#import \"Local.h\"\n  #import <A/B.h> // x\n",
            &mut out,
        );
        assert_eq!(
            out.into_iter().collect::<Vec<_>>(),
            vec!["A/B.h".to_string(), "UIKit/UIKit.h".to_string()]
        );
    }

    #[test]
    fn bare_apple_sdk_headers_are_stubbed_but_libc_is_not() {
        let mut out = BTreeSet::new();
        collect_angle_imports(
            "#import <TargetConditionals.h>\n#include <Availability.h>\n#include <stdlib.h>\n",
            &mut out,
        );
        assert_eq!(
            out.into_iter().collect::<Vec<_>>(),
            vec![
                "Availability.h".to_string(),
                "TargetConditionals.h".to_string()
            ]
        );
    }

    #[test]
    fn prelude_keeps_xctest_assertion_arguments() {
        assert!(OBJC_PRELUDE.contains("#define XCTAssertEqual(...) ((void)0, ##__VA_ARGS__)"));
        assert!(OBJC_PRELUDE.contains("@interface XCTestCase : NSObject"));
    }
}
