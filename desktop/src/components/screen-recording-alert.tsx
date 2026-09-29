import { invoke } from '@tauri-apps/api/core'
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow'
import { platform } from '@tauri-apps/plugin-os'
import { useEffect } from 'react'
import { toast } from 'sonner'
import type { PermissionStatus } from '~/lib/permissions'
import { notify } from '~/lib/notify'
import { m } from '~/paraglide/messages.js'
import { usePreferenceProvider } from '~/providers/preference'

const TOAST_ID = 'screen-recording-missing'

/**
 * On macOS, Google Meet and Cal Video are recognised by reading browser window titles, which needs
 * Screen Recording. Without it the detector sees no meeting at all and says nothing — and every
 * update of an unsigned build loses the grant. So at launch, with detection on and the permission
 * gone, say so where it is seen: a desktop notification, and the main window brought up with a
 * warning that opens the right System Settings pane.
 */
export default function ScreenRecordingAlert() {
	const { meetingDetectionEnabled } = usePreferenceProvider()

	useEffect(() => {
		if (!meetingDetectionEnabled || platform() !== 'macos') return
		let cancelled = false

		async function check() {
			const status = await invoke<PermissionStatus>('get_screen_recording_permission_status').catch(() => null)
			if (cancelled || status === null) return
			if (status === 'granted' || status === 'not_applicable') {
				toast.dismiss(TOAST_ID)
				return
			}
			// A new build is a new app to macOS, which then shows its own prompt once; it takes effect
			// only after a relaunch, so the warning below stays either way.
			const requested = await invoke<PermissionStatus>('request_screen_recording_permission').catch(() => null)
			if (cancelled || requested === 'granted') return
			void notify(m.screenRecordingMissingTitle(), m.screenRecordingMissingBody())
			const window = getCurrentWebviewWindow()
			await window.show().catch(() => undefined)
			await window.setFocus().catch(() => undefined)
			toast.warning(m.screenRecordingMissingTitle(), {
				id: TOAST_ID,
				description: m.screenRecordingMissingBody(),
				duration: Infinity,
				action: { label: m.openSystemSettings(), onClick: () => void invoke('open_screen_recording_settings') },
			})
		}

		void check()
		return () => {
			cancelled = true
		}
	}, [meetingDetectionEnabled])

	return null
}
