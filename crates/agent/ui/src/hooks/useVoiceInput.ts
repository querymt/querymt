/**
 * useVoiceInput — records audio from the microphone and sends it for STT transcription.
 *
 * Uses the MediaRecorder API to capture audio, then sends the raw bytes as a
 * binary WebSocket frame to the backend for transcription via the configured
 * STT provider+model.
 */
import { useState, useCallback, useRef, useEffect } from 'react';
import { useUiClientActions } from '../context/UiClientContext';
import { convertToWav } from '../utils/audioUtils';

export type VoiceInputState = 'idle' | 'recording' | 'transcribing';

interface UseVoiceInputOptions {
  /** STT provider name (e.g. "izwi") */
  provider: string;
  /** STT model name (e.g. "Qwen3-ASR-0.6B") */
  model: string;
  /** Called when transcription completes */
  onTranscribed: (text: string) => void;
  /** Called on error */
  onError?: (error: string) => void;
  /** Max recording duration in seconds (default: 60) */
  maxDuration?: number;
}

export function useVoiceInput({
  provider,
  model,
  onTranscribed,
  onError,
  maxDuration = 60,
}: UseVoiceInputOptions) {
  const [state, setState] = useState<VoiceInputState>('idle');
  const mediaRecorderRef = useRef<MediaRecorder | null>(null);
  const chunksRef = useRef<Blob[]>([]);
  const streamRef = useRef<MediaStream | null>(null);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const { sendTranscribe, setTranscribeCallback } = useUiClientActions();

  // Register the transcribe callback
  useEffect(() => {
    const handleResult = (text: string) => {
      setState('idle');
      onTranscribed(text);
    };
    setTranscribeCallback(handleResult);
    return () => setTranscribeCallback(null);
  }, [onTranscribed, setTranscribeCallback]);

  const stopRecording = useCallback(() => {
    if (timerRef.current) {
      clearTimeout(timerRef.current);
      timerRef.current = null;
    }
    const recorder = mediaRecorderRef.current;
    if (recorder && recorder.state !== 'inactive') {
      recorder.stop();
    }
  }, []);

  const startRecording = useCallback(async () => {
    if (state !== 'idle') return;

    try {
      const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
      streamRef.current = stream;
      chunksRef.current = [];

      // Prefer webm/opus (Chrome/Edge/Firefox), fall back to whatever is available
      const mimeType = MediaRecorder.isTypeSupported('audio/webm;codecs=opus')
        ? 'audio/webm;codecs=opus'
        : MediaRecorder.isTypeSupported('audio/webm')
          ? 'audio/webm'
          : '';

      const recorder = new MediaRecorder(stream, mimeType ? { mimeType } : undefined);
      mediaRecorderRef.current = recorder;

      recorder.ondataavailable = (e) => {
        if (e.data.size > 0) {
          chunksRef.current.push(e.data);
        }
      };

      recorder.onstop = async () => {
        // Release mic
        stream.getTracks().forEach((t) => t.stop());
        streamRef.current = null;

        const blob = new Blob(chunksRef.current, { type: recorder.mimeType || 'audio/webm' });
        chunksRef.current = [];

        if (blob.size === 0) {
          setState('idle');
          onError?.('No audio recorded');
          return;
        }

        setState('transcribing');

        try {
          // Convert browser audio (WebM/Opus, MP4/AAC, etc.) to 16kHz mono WAV
          // since izwi's ASR models expect WAV/PCM input.
          const wavBuffer = await convertToWav(blob);
          sendTranscribe(provider, model, wavBuffer, 'audio/wav');
        } catch (err) {
          setState('idle');
          onError?.(`Failed to convert/send audio: ${err}`);
        }
      };

      recorder.onerror = () => {
        setState('idle');
        stream.getTracks().forEach((t) => t.stop());
        onError?.('Recording failed');
      };

      recorder.start();
      setState('recording');

      // Auto-stop after max duration
      timerRef.current = setTimeout(() => {
        stopRecording();
      }, maxDuration * 1000);
    } catch (err) {
      setState('idle');
      if (err instanceof DOMException && err.name === 'NotAllowedError') {
        onError?.('Microphone permission denied');
      } else {
        onError?.(`Failed to start recording: ${err}`);
      }
    }
  }, [state, provider, model, sendTranscribe, onError, maxDuration, stopRecording]);

  const toggleRecording = useCallback(() => {
    if (state === 'recording') {
      stopRecording();
    } else if (state === 'idle') {
      startRecording();
    }
    // If transcribing, ignore toggle
  }, [state, startRecording, stopRecording]);

  // Cleanup on unmount
  useEffect(() => {
    return () => {
      if (timerRef.current) clearTimeout(timerRef.current);
      if (streamRef.current) {
        streamRef.current.getTracks().forEach((t) => t.stop());
      }
      if (mediaRecorderRef.current?.state !== 'inactive') {
        mediaRecorderRef.current?.stop();
      }
    };
  }, []);

  return {
    state,
    isRecording: state === 'recording',
    isTranscribing: state === 'transcribing',
    startRecording,
    stopRecording,
    toggleRecording,
  };
}
