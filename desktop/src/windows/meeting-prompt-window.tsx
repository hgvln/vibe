import { emit, listen } from '@tauri-apps/api/event'
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow'
import { Check, Mic, Volume2 } from 'lucide-react'
import { useEffect, useLayoutEffect, useState } from 'react'
import { GoogleMeetIcon, MicrosoftTeamsIcon, ZoomIcon } from '~/components/meeting-service-icons'
import { Button } from '~/components/ui/button'
import { CONFIG_KEYS } from '~/lib/config-keys'
import { readConfig } from '~/lib/config-store'
import {
	cancelAutoRecording,
	chooseRecordingScope,
	continueAutoRecording,
	dismissMeetingPrompt,
	getMeetingPromptState,
	meetingPromptReady,
	stopAutoRecording,
	type MeetingPromptState,
	type MeetingRecordingOptions,
	type MeetingSource,
	type RecordingScope,
	type UnchosenScope,
} from '~/lib/meeting-prompt'
import { supportedLanguages } from '~/lib/i18n'
import { cn } from '~/lib/style'
import { m } from '~/paraglide/messages.js'
import { getLocale, getTextDirection, setLocale } from '~/paraglide/runtime.js'
import logoUrl from '../../../design/logo.svg?url'

/** The question "record this meeting?" goes away on its own. */
const ASK_TIMEOUT_MS = 10_000
/** The notice of an automatic recording waits this long for a choice before counting down... */
const CHOICE_IDLE_MS = 30_000
/** ...then counts down this long, visibly, before the default applies. */
const CHOICE_COUNTDOWN_MS = 15_000
/** Mirrors END_GRACE in meeting_prompt.rs: the recording stops this long after the call ended. */
const END_GRACE_MS = 10_000

const serviceNames: Record<MeetingSource, string> = {
	meet: 'Google Meet',
	zoom: 'Zoom',
	teams: 'Microsoft Teams',
}

function ServiceIcon({ source }: { source: MeetingSource }) {
	const className = 'h-7 w-7'
	if (source === 'meet') return <GoogleMeetIcon className={className} />
	if (source === 'zoom') return <ZoomIcon className={className} />
	return <MicrosoftTeamsIcon className={className} />
}

function SourceChoice({
	checked,
	icon: Icon,
	label,
	disabled,
	onClick,
}: {
	checked: boolean
	icon: typeof Mic
	label: string
	disabled: boolean
	onClick: () => void
}) {
	return (
		<button
			type="button"
			aria-pressed={checked}
			disabled={disabled}
			onClick={onClick}
			className={cn(
				'flex h-7 items-center gap-1.5 rounded-md border px-2 text-[11px] font-medium transition-colors disabled:opacity-50',
				checked ? 'border-border bg-muted/35 text-foreground' : 'border-border/75 bg-transparent text-muted-foreground',
			)}>
			<span
				className={cn(
					'flex h-3.5 w-3.5 items-center justify-center rounded-[4px] border',
					checked ? 'border-primary bg-primary text-primary-foreground' : 'border-muted-foreground/55',
				)}>
				{checked && <Check className="h-2.5 w-2.5 stroke-[3]" />}
			</span>
			<Icon className="h-3 w-3" />
			<span>{label}</span>
		</button>
	)
}

/** A bar that empties as `fraction` goes from 1 to 0. */
function Countdown({ fraction }: { fraction: number }) {
	const percent = Math.round(Math.min(1, Math.max(0, fraction)) * 100)
	return (
		<div className="h-1 w-full overflow-hidden rounded-full bg-muted" role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={percent}>
			<div className="h-full rounded-full bg-primary transition-[width] duration-200 ease-linear" style={{ width: `${percent}%` }} />
		</div>
	)
}

/** Milliseconds since `since`, refreshed a few times a second; 0 while there is nothing to time. */
function useElapsed(since: number | null) {
	const [elapsed, setElapsed] = useState(0)
	useEffect(() => {
		if (since === null) {
			setElapsed(0)
			return
		}
		setElapsed(Date.now() - since)
		const timer = window.setInterval(() => setElapsed(Date.now() - since), 200)
		return () => window.clearInterval(timer)
	}, [since])
	return elapsed
}

function Header({ source, title, description }: { source: MeetingSource; title: string; description: string }) {
	return (
		<div className="flex min-w-0 items-center gap-2.5">
			<div className="relative flex h-9 w-12 shrink-0 items-center">
				<ServiceIcon source={source} />
				<img src={logoUrl} alt="" className="absolute end-0 h-5 w-5 rounded-full border-2 border-card" />
			</div>
			<div className="min-w-0">
				<h1 className="truncate text-sm font-semibold leading-5">{title}</h1>
				<p className="truncate text-xs text-muted-foreground">{description}</p>
			</div>
		</div>
	)
}

export default function MeetingPromptWindow() {
	const configuredLocale = readConfig(CONFIG_KEYS.displayLanguage, 'en-US')
	if (supportedLanguages[configuredLocale] && configuredLocale !== getLocale()) {
		setLocale(configuredLocale as never, { reload: false })
	}

	const [state, setState] = useState<MeetingPromptState | null>(null)
	const [busy, setBusy] = useState(false)
	const [sources, setSources] = useState<MeetingRecordingOptions>({ microphone: true, systemAudio: true })
	// When the current state appeared: the notice and the end-of-call question are timed from it.
	const [shownAt, setShownAt] = useState<number | null>(null)
	const elapsed = useElapsed(shownAt)
	const theme = readConfig<'light' | 'dark'>(CONFIG_KEYS.theme, window.matchMedia?.('(prefers-color-scheme: dark)').matches ? 'dark' : 'light')
	const direction = getTextDirection(configuredLocale)
	const unchosen = readConfig<UnchosenScope>(CONFIG_KEYS.autoRecordUnchosenScope, 'personal')
	const sharedLabel = readConfig<string>(CONFIG_KEYS.sharedScopeLabel, '').trim() || m.meetingPromptScopeShared()

	useLayoutEffect(() => {
		document.title = m.appTitle()
		document.body.dir = direction
		document.documentElement.classList.toggle('dark', theme === 'dark')
		document.documentElement.classList.add('meeting-prompt-window')
		document.documentElement.style.background = 'transparent'
		document.body.style.background = 'transparent'
		void getCurrentWebviewWindow()
			.setTheme(theme)
			.catch(() => undefined)
		return () => {
			document.documentElement.classList.remove('meeting-prompt-window')
			document.documentElement.style.removeProperty('background')
			document.body.style.removeProperty('background')
		}
	}, [direction, theme])

	useEffect(() => {
		let cancelled = false
		const unlisten = listen<MeetingPromptState>('meeting-prompt-state', ({ payload }) => {
			if (!cancelled) setState(payload)
		})
		void (async () => {
			try {
				const initialState = await getMeetingPromptState()
				if (!cancelled) setState(initialState)
			} catch (error) {
				console.error('Failed to initialize meeting prompt:', error)
			}
			await meetingPromptReady().catch((error) => console.error('Failed to mark meeting prompt ready:', error))
		})()
		return () => {
			cancelled = true
			unlisten.then((dispose) => dispose())
		}
	}, [])

	useEffect(() => {
		setShownAt(state ? Date.now() : null)
		setBusy(false)
	}, [state])

	// The main window reports back after the question's "Record": the question is done. The
	// notice of an automatic recording is not a question and stays until it is answered.
	useEffect(() => {
		const unlisten = listen<{ started: boolean }>('meeting-prompt-recording-result', ({ payload }) => {
			setBusy(false)
			if (payload.started) setState((current) => (current?.mode === 'ask' ? null : current))
		})
		return () => {
			unlisten.then((dispose) => dispose())
		}
	}, [])

	// The window is transparent and stays where it is once the content is gone; hide it with
	// the content so it cannot sit invisibly over the meeting.
	useEffect(() => {
		if (state) return
		void getCurrentWebviewWindow()
			.hide()
			.catch(() => undefined)
	}, [state])

	// The question goes away on its own; the end-of-call question is closed by the recording
	// stopping (the backend hides the window), this is only a safety net.
	useEffect(() => {
		if (!state || state.mode === 'recording') return
		const delay = state.mode === 'ask' ? ASK_TIMEOUT_MS : END_GRACE_MS + 1_000
		const timeout = window.setTimeout(() => {
			void dismissMeetingPrompt()
				.catch((error) => console.error('Failed to auto-dismiss meeting prompt:', error))
				.finally(() => setState(null))
		}, delay)
		return () => window.clearTimeout(timeout)
	}, [state])

	// Nobody chose where the transcript goes: the setting decides, once the countdown ran out.
	useEffect(() => {
		if (state?.mode !== 'recording' || busy || elapsed < CHOICE_IDLE_MS + CHOICE_COUNTDOWN_MS) return
		setBusy(true)
		const apply = unchosen === 'discard' ? cancelAutoRecording() : chooseRecordingScope('personal')
		void apply.catch((error) => console.error('Failed to apply the default for the unanswered notice:', error)).finally(() => setState(null))
	}, [busy, elapsed, state, unchosen])

	async function dismiss() {
		if (busy) return
		setBusy(true)
		try {
			await dismissMeetingPrompt()
			setState(null)
		} catch (error) {
			console.error('Failed to dismiss meeting prompt:', error)
			setBusy(false)
		}
	}

	async function startRecording() {
		if (busy) return
		setBusy(true)
		try {
			await emit('meeting-prompt-start-recording', sources)
		} catch (error) {
			console.error('Failed to start recording from meeting prompt:', error)
			setBusy(false)
		}
	}

	/** One of the backend commands that answer a notice; the window goes away with the answer. */
	async function answer(action: () => Promise<void>, what: string) {
		if (busy) return
		setBusy(true)
		try {
			await action()
			setState(null)
		} catch (error) {
			console.error(`Failed to ${what}:`, error)
			setBusy(false)
		}
	}

	const chooseScope = (scope: RecordingScope) => answer(() => chooseRecordingScope(scope), 'choose where the transcript goes')
	const cancelRecording = () => answer(cancelAutoRecording, 'cancel the automatic recording')
	const keepGoing = () => answer(continueAutoRecording, 'keep the automatic recording going')
	const stopNow = () => answer(stopAutoRecording, 'stop the automatic recording')

	function toggleSource(source: keyof MeetingRecordingOptions) {
		setSources((current) => {
			if (current[source] && Object.values(current).filter(Boolean).length === 1) return current
			return { ...current, [source]: !current[source] }
		})
	}

	useEffect(() => {
		function onKeyDown(event: KeyboardEvent) {
			if (event.key === 'Escape') void dismiss()
		}
		window.addEventListener('keydown', onKeyDown)
		return () => window.removeEventListener('keydown', onKeyDown)
	})

	if (!state) return null

	const buttonClass = 'h-8 rounded-md px-2.5 text-xs'

	if (state.mode === 'recording') {
		const remaining = Math.max(0, CHOICE_IDLE_MS + CHOICE_COUNTDOWN_MS - elapsed)
		const countingDown = elapsed >= CHOICE_IDLE_MS
		const seconds = String(Math.ceil(remaining / 1000))
		const description = !countingDown
			? m.meetingPromptRecordingDescription()
			: unchosen === 'discard'
				? m.meetingPromptUnchosenDiscard({ seconds })
				: m.meetingPromptUnchosenPersonal({ seconds })
		return (
			<div className="flex h-screen w-screen items-center justify-center bg-transparent p-2">
				<section className="flex h-full w-full flex-col gap-2 rounded-xl border border-border/70 bg-card px-3 py-2.5 text-card-foreground shadow-xl">
					<Header source={state.source} title={m.meetingPromptRecordingTitle({ source: serviceNames[state.source] })} description={description} />
					{countingDown && <Countdown fraction={remaining / CHOICE_COUNTDOWN_MS} />}
					<div className="mt-auto flex justify-end gap-1.5 border-t border-border/55 pt-2">
						<Button type="button" variant="ghost" size="sm" className={buttonClass} disabled={busy} onClick={() => void cancelRecording()}>
							{m.meetingPromptDontRecord()}
						</Button>
						<Button type="button" variant="outline" size="sm" className={buttonClass} disabled={busy} onClick={() => void chooseScope('personal')}>
							{m.meetingPromptScopePersonal()}
						</Button>
						<Button type="button" size="sm" className={buttonClass} disabled={busy} onClick={() => void chooseScope('shared')}>
							{sharedLabel}
						</Button>
					</div>
				</section>
			</div>
		)
	}

	if (state.mode === 'ending') {
		const remaining = Math.max(0, END_GRACE_MS - elapsed)
		return (
			<div className="flex h-screen w-screen items-center justify-center bg-transparent p-2">
				<section className="flex h-full w-full flex-col gap-2 rounded-xl border border-border/70 bg-card px-3 py-2.5 text-card-foreground shadow-xl">
					<Header
						source={state.source}
						title={m.meetingPromptEndingTitle({ source: serviceNames[state.source] })}
						description={m.meetingPromptEndingDescription({ seconds: String(Math.ceil(remaining / 1000)) })}
					/>
					<Countdown fraction={remaining / END_GRACE_MS} />
					<div className="mt-auto flex justify-end gap-1.5 border-t border-border/55 pt-2">
						<Button type="button" variant="ghost" size="sm" className={buttonClass} disabled={busy} onClick={() => void keepGoing()}>
							{m.meetingPromptKeepGoing()}
						</Button>
						<Button type="button" size="sm" className={buttonClass} disabled={busy} onClick={() => void stopNow()}>
							{m.meetingPromptStopNow()}
						</Button>
					</div>
				</section>
			</div>
		)
	}

	return (
		<div className="flex h-screen w-screen items-center justify-center bg-transparent p-2">
			<section className="flex h-full w-full flex-col gap-2 rounded-xl border border-border/70 bg-card px-3 py-2.5 text-card-foreground shadow-xl">
				<Header source={state.source} title={m.meetingPromptTitle({ source: serviceNames[state.source] })} description={m.meetingPromptDescription()} />
				<div className="flex gap-1.5" aria-label={m.recordingControls()}>
					<SourceChoice checked={sources.microphone} icon={Mic} label={m.microphone()} disabled={busy} onClick={() => toggleSource('microphone')} />
					<SourceChoice
						checked={sources.systemAudio}
						icon={Volume2}
						label={m.systemAudioPermission()}
						disabled={busy}
						onClick={() => toggleSource('systemAudio')}
					/>
				</div>
				<div className="mt-auto flex justify-end gap-1.5 border-t border-border/55 pt-2">
					<Button type="button" variant="ghost" size="sm" className="h-7 rounded-md px-2 text-xs" disabled={busy} onClick={() => void dismiss()}>
						{m.meetingPromptDismiss()}
					</Button>
					<Button type="button" size="sm" className="h-8 rounded-md px-3 text-xs" disabled={busy} onClick={() => void startRecording()}>
						{m.meetingPromptStart()}
					</Button>
				</div>
			</section>
		</div>
	)
}
