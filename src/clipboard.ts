import { writeText as writeNativeClipboardText } from "@tauri-apps/plugin-clipboard-manager";

export async function writeClipboardText(text: string): Promise<void> {
  try {
    await writeNativeClipboardText(text);
  } catch (nativeError) {
    if (!navigator.clipboard?.writeText) {
      throw nativeError;
    }
    await navigator.clipboard.writeText(text);
  }
}
