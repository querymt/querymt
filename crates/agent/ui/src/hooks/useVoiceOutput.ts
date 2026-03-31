/**
 * useVoiceOutput — sends text for TTS synthesis and plays the resulting audio.
 *
 * Sends a JSON text frame to the backend requesting speech synthesis. The
 * response arrives as a binary WebSocket frame containing raw audio bytes,
 * which are played via the Web Audio API.
 */
import { useState, useCallback, useRef, useEffect } from 'react';
import { useUiClientActions } from '../context/UiClientContext';

export type VoiceOutputState = 'idle' | 'synthesizing' | 'playing';

interface UseVoiceOutputOptions {
  /** TTS provider name (e.g. "izwi") */
  provider: string;
  /** TTS model name (e.g. "Kokoro-82M") */
  model: string;
  /** Optional voice preset name */
  voice?: string;
  /** Optional audio format (default: "wav") */
  format?: string;
  /** Called on error */
  onError?: (error: string) => void;
}

export function useVoiceOutput({
  provider,
  model,
  voice,
  format,
  onError,
}: UseVoiceOutputOptions) {
  const [state, setState] = useState<VoiceOutputState>('idle');
  const audioRef = useRef<HTMLAudioElement | null>(null);
  const objectUrlRef = useRef<string | null>(null);
  const { sendSpeech, setSpeechCallback, setSpeechErrorCallback } = useUiClientActions();

  // Clean up any active object URL
  const cleanupAudio = useCallback(() => {
    if (audioRef.current) {
      audioRef.current.pause();
      audioRef.current.removeAttribute('src');
      audioRef.current = null;
    }
    if (objectUrlRef.current) {
      URL.revokeObjectURL(objectUrlRef.current);
      objectUrlRef.current = null;
    }
  }, []);

  // Register speech result callback
  useEffect(() => {
    const handleAudio = (audioData: ArrayBuffer, mimeType: string) => {
      cleanupAudio();

      const blob = new Blob([audioData], { type: mimeType });
      const url = URL.createObjectURL(blob);
      objectUrlRef.current = url;

      const audio = new Audio(url);
      audioRef.current = audio;

      audio.onplay = () => setState('playing');
      audio.onended = () => {
        setState('idle');
        cleanupAudio();
      };
      audio.onerror = () => {
        setState('idle');
        cleanupAudio();
        onError?.('Audio playback failed');
      };

      setState('playing');
      audio.play().catch((err) => {
        setState('idle');
        cleanupAudio();
        onError?.(`Playback failed: ${err}`);
      });
    };

    setSpeechCallback(handleAudio);
    return () => setSpeechCallback(null);
  }, [cleanupAudio, onError, setSpeechCallback]);

  // Register speech error callback
  useEffect(() => {
    const handleError = (error: string) => {
      setState('idle');
      onError?.(error);
    };
    setSpeechErrorCallback(handleError);
    return () => setSpeechErrorCallback(null);
  }, [onError, setSpeechErrorCallback]);

  /** Send text for speech synthesis. */
  const speak = useCallback((text: string) => {
    console.log('[TTS] speak called', { state, provider, model, textLen: text.length });
    if (state === 'synthesizing') {
      console.log('[TTS] speak: already synthesizing, skipping');
      return;
    }
    cleanupAudio();
    setState('synthesizing');
    sendSpeech(provider, model, text, voice, format);
  }, [state, provider, model, voice, format, sendSpeech, cleanupAudio]);

  /** Stop current playback. */
  const stop = useCallback(() => {
    cleanupAudio();
    setState('idle');
  }, [cleanupAudio]);

  // Cleanup on unmount
  useEffect(() => {
    return () => {
      cleanupAudio();
    };
  }, [cleanupAudio]);

  return {
    state,
    isSynthesizing: state === 'synthesizing',
    isPlaying: state === 'playing',
    isBusy: state !== 'idle',
    speak,
    stop,
  };
}
