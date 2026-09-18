import { invoke } from '@tauri-apps/api/core'

export type MeetingSource = 'meet' | 'zoom' | 'teams'

/** `ask` offers to record; `recording` says a recording already started on its own. */
export type MeetingPromptMode = 'ask' | 'recording'

export interface MeetingPromptState {
	source: MeetingSource
	mode: MeetingPromptMode
}

export interface MeetingRecordingOptions {
	microphone: boolean
	systemAudio: boolean
}

export const getMeetingDetectionEnabled = () => invoke<boolean>('get_meeting_detection_enabled')

export const setMeetingDetectionEnabled = (enabled: boolean) => invoke<void>('set_meeting_detection_enabled', { enabled })

export const getMeetingPromptState = () => invoke<MeetingPromptState | null>('get_meeting_prompt_state')

export const dismissMeetingPrompt = () => invoke<void>('dismiss_meeting_prompt')

/** Stop the recording that started on its own and discard its audio. */
export const cancelAutoRecording = () => invoke<void>('cancel_auto_recording')

export const meetingPromptReady = () => invoke<void>('meeting_prompt_ready')
