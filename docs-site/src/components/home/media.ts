// Media slots for the standalone home pages (src/components/home/Home.astro).
//
// Reserved architecture animation video slot:
// Populate `ARCHITECTURE_VIDEO_URL` with the GitHub issue #3 user-attachments
// video URL when ready. While empty, the homepage renders the architectural
// diagram stage in the same 16:9 slot; once populated, it renders a looping
// <video> player with play/pause controls and falls back to `ARCHITECTURE_POSTER_URL`
// if supplied.
export const ARCHITECTURE_VIDEO_URL = '';
export const ARCHITECTURE_POSTER_URL = '';
