import { create } from 'zustand';

const STORAGE_KEY = 'querymt_voice_settings';

export interface VoiceSettings {
  /** STT provider name (e.g. "izwi") */
  sttProvider: string;
  /** STT model name (e.g. "Qwen3-ASR-0.6B") */
  sttModel: string;
  /** TTS provider name (e.g. "izwi") */
  ttsProvider: string;
  /** TTS model name (e.g. "Kokoro-82M") */
  ttsModel: string;
  /** TTS voice preset (e.g. "af_heart") */
  ttsVoice: string;
  /** Auto-speak agent responses when voice mode is active */
  autoTtsEnabled: boolean;
}

interface VoiceState extends VoiceSettings {
  setSttConfig: (provider: string, model: string) => void;
  setTtsConfig: (provider: string, model: string) => void;
  setTtsVoice: (voice: string) => void;
  setAutoTtsEnabled: (enabled: boolean) => void;
}

function loadFromStorage(): Partial<VoiceSettings> {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw) return JSON.parse(raw);
  } catch { /* localStorage unavailable */ }
  return {};
}

function saveToStorage(settings: VoiceSettings) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(settings));
  } catch { /* localStorage unavailable */ }
}

const defaults: VoiceSettings = {
  sttProvider: 'izwi',
  sttModel: 'Qwen3-ASR-0.6B',
  ttsProvider: 'izwi',
  ttsModel: 'Qwen3-TTS-12Hz-0.6B-Base-4bit',
  ttsVoice: '',
  autoTtsEnabled: false,
};

export const useVoiceStore = create<VoiceState>((set, get) => {
  const persisted = loadFromStorage();
  const initial = { ...defaults, ...persisted };

  return {
    ...initial,

    setSttConfig: (provider, model) => {
      set({ sttProvider: provider, sttModel: model });
      saveToStorage({ ...get(), sttProvider: provider, sttModel: model });
    },

    setTtsConfig: (provider, model) => {
      set({ ttsProvider: provider, ttsModel: model });
      saveToStorage({ ...get(), ttsProvider: provider, ttsModel: model });
    },

    setTtsVoice: (voice) => {
      set({ ttsVoice: voice });
      saveToStorage({ ...get(), ttsVoice: voice });
    },

    setAutoTtsEnabled: (enabled) => {
      set({ autoTtsEnabled: enabled });
      saveToStorage({ ...get(), autoTtsEnabled: enabled });
    },
  };
});
