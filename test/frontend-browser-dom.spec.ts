import { expect, test } from "@playwright/test";

const HARNESS_PATH = "/browser-dom-harness.html";

type HarnessAudioTrack = {
  index: number;
  codec: string;
  channels: number;
  sample_rate: number;
  language?: string;
  title?: string;
};

async function mountPreviewPipe(
  page: import("@playwright/test").Page,
  audioTracks: HarnessAudioTrack[] = [
    {
      index: 0,
      codec: "aac",
      channels: 2,
      sample_rate: 48000,
      language: "eng",
      title: "Main Mix",
    },
    {
      index: 1,
      codec: "aac",
      channels: 2,
      sample_rate: 48000,
      language: "spa",
      title: "Commentary",
    },
  ],
): Promise<void> {
  await page.goto(HARNESS_PATH);
  await page.evaluate(async (inputAudioTracks) => {
    const container = document.getElementById("video-player");
    if (!container) throw new Error("no container");

    const pipe = {
      id: "browser-dom-pipe",
      name: "Browser DOM Pipe",
      key: "browser_dom_key",
      inputSource: null,
      ingestUrls: { rtmp: null, srt: null },
      input: {
        status: "on",
        time: null,
        video: { codec: "h264", width: 1920, height: 1080, fps: 30 },
        audio: { codec: "aac", channels: 2, sample_rate: 48000 },
        audioTracks: inputAudioTracks,
        bytesReceived: 0,
        bytesSent: 0,
        readers: 0,
        bitrateKbps: null,
        publisher: null,
        unexpectedReadersCount: 0,
      },
      outs: [],
      stats: {
        inputBitrateKbps: null,
        outputBitrateKbps: null,
        readerCount: 0,
        outputCount: 0,
        readerMismatch: false,
        unexpectedReadersCount: 0,
      },
      recording: { enabled: false, active: false },
    };

    class FakeHls {
      static Events = {
        MANIFEST_PARSED: "manifestParsed",
        AUDIO_TRACKS_UPDATED: "audioTracksUpdated",
        ERROR: "error",
      };

      static isSupported() {
        return true;
      }

      handlers: Record<string, ((event: string, data?: unknown) => void)[]> =
        {};
      audioTrack = 0;

      constructor() {
        (
          window as typeof window & { __previewTestHls?: FakeHls }
        ).__previewTestHls = this;
      }

      loadSource(_src: string) {}
      attachMedia(_video: HTMLVideoElement) {}
      destroy() {}

      on(event: string, handler: (event: string, data?: unknown) => void) {
        (this.handlers[event] ||= []).push(handler);
      }

      emit(event: string, data?: unknown) {
        for (const handler of this.handlers[event] || []) {
          handler(event, data);
        }
      }
    }

    (
      window as typeof window & {
        Hls: typeof FakeHls;
      }
    ).Hls = FakeHls;
    window.fetch = async () =>
      new Response("#EXTM3U\n#EXT-X-VERSION:3\n", {
        status: 200,
        headers: { "content-type": "application/vnd.apple.mpegurl" },
      });

    const { renderInputPreview } = await import("/js/features/input-preview.js");
    renderInputPreview(container, pipe as never);
  }, audioTracks);
}

test.describe("Frontend Browser DOM", () => {
  test("preview audio picker opens and switches tracks without the full app server", async ({
    page,
  }) => {
    await mountPreviewPipe(page);

    const playBtn = page.locator("#video-player button", {
      hasText: "Play preview",
    });
    await expect(playBtn).toBeVisible();
    await playBtn.click();

    await page.waitForFunction(
      () =>
        Boolean(
          (
            window as typeof window & {
              __previewTestHls?: unknown;
            }
          ).__previewTestHls,
        ),
    );
    await page.evaluate(() => {
      const testWindow = window as typeof window & {
        Hls: { Events: Record<string, string> };
        __previewTestHls?: {
          emit: (event: string, data?: unknown) => void;
          audioTrack: number;
        };
      };
      testWindow.__previewTestHls?.emit(
        testWindow.Hls.Events.AUDIO_TRACKS_UPDATED,
        {
          audioTracks: [
            { id: 0, name: "Main Mix", lang: "eng" },
            { id: 1, name: "Commentary", lang: "spa" },
          ],
        },
      );
    });

    const audioPickerButton = page.locator(
      '#video-player button[aria-haspopup="listbox"]',
    );
    await expect(audioPickerButton).toBeVisible();
    await expect(audioPickerButton).toHaveText("Audio: Main Mix");
    await expect(audioPickerButton).toHaveAttribute("aria-expanded", "false");

    await audioPickerButton.click();
    await expect(audioPickerButton).toHaveAttribute("aria-expanded", "true");

    const commentaryOption = page
      .locator('[role="option"]')
      .filter({ hasText: "Commentary" })
      .first();
    await expect(commentaryOption).toBeVisible();
    await commentaryOption.click();

    await expect(audioPickerButton).toHaveText("Audio: Commentary");
    await expect(audioPickerButton).toHaveAttribute("aria-expanded", "false");

    const selectedTrack = await page.evaluate(() => {
      const testWindow = window as typeof window & {
        __previewTestHls?: { audioTrack: number };
      };
      return testWindow.__previewTestHls?.audioTrack ?? null;
    });
    expect(selectedTrack).toBe(1);
  });

  test("preview audio picker surfaces all high-index tracks and switches to the last one", async ({
    page,
  }) => {
    const audioTracks = Array.from({ length: 16 }, (_, index) => ({
      index,
      codec: "aac",
      channels: index % 2 === 0 ? 2 : 1,
      sample_rate: 48000,
      language: `lang${index}`,
      title: `Track ${index + 1}`,
    }));
    await mountPreviewPipe(page, audioTracks);

    const playBtn = page.locator("#video-player button", {
      hasText: "Play preview",
    });
    await expect(playBtn).toBeVisible();
    await playBtn.click();

    await page.waitForFunction(
      () =>
        Boolean(
          (
            window as typeof window & {
              __previewTestHls?: unknown;
            }
          ).__previewTestHls,
        ),
    );
    await page.evaluate(() => {
      const testWindow = window as typeof window & {
        Hls: { Events: Record<string, string> };
        __previewTestHls?: {
          emit: (event: string, data?: unknown) => void;
          audioTrack: number;
        };
      };
      testWindow.__previewTestHls?.emit(
        testWindow.Hls.Events.AUDIO_TRACKS_UPDATED,
        {
          audioTracks: Array.from({ length: 16 }, (_, index) => ({
            id: index,
            name: `Track ${index + 1}`,
            lang: `lang${index}`,
          })),
        },
      );
    });

    const audioPickerButton = page.locator(
      '#video-player button[aria-haspopup="listbox"]',
    );
    await expect(audioPickerButton).toBeVisible();
    await audioPickerButton.click();

    const track16Option = page
      .locator('[role="option"]')
      .filter({ hasText: "Track 16" })
      .first();
    await expect(track16Option).toBeVisible();
    await track16Option.click();

    await expect(audioPickerButton).toHaveText("Audio: Track 16");

    const selectedTrack = await page.evaluate(() => {
      const testWindow = window as typeof window & {
        __previewTestHls?: { audioTrack: number };
      };
      return testWindow.__previewTestHls?.audioTrack ?? null;
    });
    expect(selectedTrack).toBe(15);
  });
});
