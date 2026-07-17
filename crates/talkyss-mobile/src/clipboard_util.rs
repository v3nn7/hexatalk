//! Read system clipboard. On Android, egui/winit has no OS clipboard — use JNI.

/// Best-effort clipboard text (plain).
pub fn get_text() -> Option<String> {
    #[cfg(target_os = "android")]
    {
        android_get_text()
    }
    #[cfg(not(target_os = "android"))]
    {
        // Desktop: try arboard if available via env; otherwise None (user can Ctrl+V).
        None
    }
}

#[cfg(target_os = "android")]
fn android_get_text() -> Option<String> {
    use jni::objects::{JObject, JString, JValue};
    use jni::JavaVM;

    let ctx = ndk_context::android_context();
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()).ok()? };
    let mut env = vm.attach_current_thread().ok()?;
    let context = unsafe { JObject::from_raw(ctx.context().cast()) };

    // context.getSystemService(Context.CLIPBOARD_SERVICE)
    let context_class = env.find_class("android/content/Context").ok()?;
    let clipboard_service = env
        .get_static_field(context_class, "CLIPBOARD_SERVICE", "Ljava/lang/String;")
        .ok()?
        .l()
        .ok()?;
    let service = env
        .call_method(
            &context,
            "getSystemService",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[JValue::Object(&clipboard_service)],
        )
        .ok()?
        .l()
        .ok()?;
    if service.is_null() {
        return None;
    }

    // clipboard.hasPrimaryClip()
    let has = env
        .call_method(&service, "hasPrimaryClip", "()Z", &[])
        .ok()?
        .z()
        .ok()?;
    if !has {
        return None;
    }

    // clip = getPrimaryClip()
    let clip = env
        .call_method(
            &service,
            "getPrimaryClip",
            "()Landroid/content/ClipData;",
            &[],
        )
        .ok()?
        .l()
        .ok()?;
    if clip.is_null() {
        return None;
    }

    let count = env
        .call_method(&clip, "getItemCount", "()I", &[])
        .ok()?
        .i()
        .ok()?;
    if count <= 0 {
        return None;
    }

    let item = env
        .call_method(
            &clip,
            "getItemAt",
            "(I)Landroid/content/ClipData$Item;",
            &[JValue::Int(0)],
        )
        .ok()?
        .l()
        .ok()?;
    if item.is_null() {
        return None;
    }

    // coerceToText(context)
    let text_obj = env
        .call_method(
            &item,
            "coerceToText",
            "(Landroid/content/Context;)Ljava/lang/CharSequence;",
            &[JValue::Object(&context)],
        )
        .ok()?
        .l()
        .ok()?;
    if text_obj.is_null() {
        return None;
    }

    let jstr = env
        .call_method(&text_obj, "toString", "()Ljava/lang/String;", &[])
        .ok()?
        .l()
        .ok()?;
    let jstring = JString::from(jstr);
    let rust = env.get_string(&jstring).ok()?;
    let s: String = rust.into();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}
