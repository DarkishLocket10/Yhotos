import {enhance} from "./lib/lifecycle.js";
import {enhanceVideo} from "./lib/video.js";
enhance(signal => {
  document.querySelectorAll<HTMLVideoElement>("video[data-video]").forEach(video => enhanceVideo(video, video.dataset.scrub || "", Number(video.dataset.duration), signal));
});
