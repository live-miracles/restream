// function getRtmpStats(type) {
//     if (statsJson === null) {
//         return [];
//     }
//     let outputs = statsJson.rtmp.server.application.find((app) => app.name['#text'] == type).live
//         .stream;
//     if (outputs === undefined) outputs = []; // no streams
//     if (!Array.isArray(outputs)) outputs = [outputs]; // only one stream

//     return outputs.map((s) => {
//         const streamNo = s.name['#text'].split('-')[0].replace('stream', '');
//         const name = s.name['#text'].split('-')[1];
//         let id = null;
//         if (type === 'output') {
//             id =
//                 streamOutsConfig[parseInt(streamNo)].find((o) => o && o.name === name)?.out ?? name;
//         }

//         return {
//             id: id,
//             input: streamNo,
//             time: parseInt(s.time['#text']),
//             video: {
//                 codec: s.meta?.video?.codec?.['#text'],
//                 fps: s.meta?.video?.frame_rate?.['#text'],
//                 height: s.meta?.video?.height?.['#text'],
//                 width: s.meta?.video?.width?.['#text'],
//                 level: s.meta?.video?.level?.['#text'],
//                 profile: s.meta?.video?.profile?.['#text'],
//                 bw: s.bw_video?.['#text'],
//             },
//             audio: {
//                 codec: s.meta?.audio?.codec?.['#text'],
//                 profile: s.meta?.audio?.profile?.['#text'],
//                 channels: s.meta?.audio?.channels?.['#text'],
//                 sample_rate: s.meta?.audio?.sample_rate?.['#text'],
//                 bw: s.bw_audio?.['#text'],
//             },
//         };
//     });
// }

function parsePipelinesInfo() {
    const newPipelines = [];

    config?.pipelines.forEach((p) =>
        newPipelines.push({
            id: p.id,
            name: p.name,
            key: p.streamKey,
            input: { status: 'off', time: null, video: null, audio: null },
            outs: p.outputs.map((out) => ({
                id: out.id,
                pipe: p.name,
                name: out.name,
                encoding: 'none',
                url: out.url,
                status: 'off', // status = processes?.includes(pipe.id + 'out' + out.out) ? 'error' : 'off';
                time: null,
                video: null,
                audio: null,
            })),
        }),
    );

    // getRtmpStats('output').forEach((s) => {
    //     let pipe = newPipelines.find((p) => p.key === 'stream' + s.input);
    //     if (!pipe) {
    //         console.error('Pipeline not found for stats', s);
    //         pipe = {
    //             id: String(s.input),
    //             name: 'stream' + s.input,
    //             key: 'stream' + s.input,
    //             input: {
    //                 status: 'warning',
    //                 time: 0,
    //                 video: null,
    //                 audio: null,
    //             },
    //             outs: [],
    //         };
    //         newPipelines.push(pipe);
    //     }

    //     let out = pipe.outs.find((o) => o.id === s.id);
    //     if (!out) {
    //         console.error('Output not found for stats', s);
    //         out = { id: null, pipe: pipe.name, name: s.id, status: 'warning' };
    //         console.log(pipe);
    //         pipe.outs.push(out);
    //     } else {
    //         out.status = s.video.bw > 0 ? 'on' : 'warning';
    //     }

    //     out.time = s.time;
    //     out.video = s.video;
    //     out.audio = s.audio;
    // });

    // getRtmpStats('distribute').forEach((s) => {
    //     let pipe = newPipelines.find((p) => p.key === 'stream' + s.input);
    //     if (!pipe) {
    //         console.error('Pipeline not found for stats', s);
    //         pipe = {
    //             id: String(s.input),
    //             name: 'stream' + s.input,
    //             key: 'stream' + s.input,
    //             input: {
    //                 status: 'warning',
    //                 time: 0,
    //                 video: null,
    //                 audio: null,
    //             },
    //             outs: [],
    //         };
    //         newPipelines.push(pipe);
    //     }

    //     pipe.input = {
    //         status: s.video.bw > 0 ? 'on' : 'warning',
    //         time: s.time,
    //         video: s.video,
    //         audio: s.audio,
    //     };
    // });

    return newPipelines;
}
