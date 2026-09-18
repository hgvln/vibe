import { invoke } from '@tauri-apps/api/core'

export type MeetingSource = 'meet' | 'zoom' | 'teams'

/**
 * `ask` offers to record; `recording` says a recording already started on its own and asks
 * where its transcript should go; `ending` says the call looks over and asks whether to stop.
 */
export type MeetingPromptMode = 'ask' | 'recording' | 'ending'

/** Where the transcript of an automatic recording goes: the shared auto-export destination or the personal folder. */
export type RecordingScope = 'shared' | 'personal'

/** What an automatic recording becomes when nobody chose on its notice. */
export type UnchosenScope = 'personal' | 'discard'

export interface FinishedAutoRecording {
	/** null when nobody chose before the call ended. */
	scope: RecordingScope | null
}

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

/** Where the transcript of the recording that started on its own should go. */
export const chooseRecordingScope = (scope: RecordingScope) => invoke<void>('choose_recording_scope', { scope })

/** The call is not over after all: keep recording until the meeting is seen again and ends. */
export const continueAutoRecording = () => invoke<void>('continue_auto_recording')

/** The call is over: stop the automatic recording now. */
export const stopAutoRecording = () => invoke<void>('stop_auto_recording')

/** The automatic recording that just stopped, once; null for an ordinary recording. */
export const takeFinishedAutoRecording = () => invoke<FinishedAutoRecording | null>('take_finished_auto_recording')

export const meetingPromptReady = () => invoke<void>('meeting_prompt_ready')
