/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

/// # Brush vertex shaders memory layout
///
/// The overall memory layout is the same for all brush shaders.
///
/// The vertex shader receives a minimal amount of data from vertex attributes (packed into a single
/// ivec4 per instance) and the rest is fetched from various uniform samplers using offsets decoded
/// from the vertex attributes.
///
/// The diagram below shows the the various pieces of data fectched in the vertex shader:
///
///```ascii
///                                                                         (sPrimitiveHeadersI)
///                          (VBO)                                     +-----------------------+
/// +----------------------------+      +----------------------------> | Int header            |
/// | Instance vertex attributes |      |        (sPrimitiveHeadersF)  |                       |
/// |                            |      |     +---------------------+  |   z                   |
/// | x: prim_header_address    +-------+---> | Float header        |  |   specific_address  +-----+
/// | y: picture_task_address   +---------+   |                     |  |   transform_address +---+ |
/// |    clip_address           +-----+   |   |    local_rect       |  |   user_data           | | |
/// | z: flags                   |    |   |   |    local_clip_rect  |  +-----------------------+ | |
/// |    segment_index           |    |   |   +---------------------+                            | |
/// | w: resource_address       +--+  |   |                                                      | |
/// +----------------------------+ |  |   |                                 (sGpuCache)          | |
///                                |  |   |         (sGpuCache)          +------------+          | |
///                                |  |   |   +---------------+          | Transform  | <--------+ |
///                (sGpuCache)     |  |   +-> | Picture task  |          +------------+            |
///            +-------------+     |  |       |               |                                    |
///            |  Resource   | <---+  |       |         ...   |                                    |
///            |             |        |       +---------------+   +--------------------------------+
///            |             |        |                           |
///            +-------------+        |             (sGpuCache)   v                        (sGpuCache)
///                                   |       +---------------+  +--------------+---------------+-+-+
///                                   +-----> | Clip area     |  | Brush data   |  Segment data | | |
///                                           |               |  |              |               | | |
///                                           |         ...   |  |         ...  |          ...  | | | ...
///                                           +---------------+  +--------------+---------------+-+-+
///```
///
/// - Segment data address is obtained by combining the address stored in the int header and the
///   segment index decoded from the vertex attributes.
/// - Resource data is optional, some brush types (such as images) store some extra data there while
///   other brush types don't use it.
///


#ifdef WR_VERTEX_SHADER

void brush_vs(
    VertexInfo vi,
    int prim_address,
    RectWithSize local_rect,
    RectWithSize segment_rect,
    ivec4 prim_user_data,
    int specific_resource_address,
    mat4 transform,
    PictureTask pic_task,
    int brush_flags,
    vec4 segment_data
);

#define VECS_PER_SEGMENT                    2

#define BRUSH_FLAG_PERSPECTIVE_INTERPOLATION    1
#define BRUSH_FLAG_SEGMENT_RELATIVE             2
#define BRUSH_FLAG_SEGMENT_REPEAT_X             4
#define BRUSH_FLAG_SEGMENT_REPEAT_Y             8
#define BRUSH_FLAG_TEXEL_RECT                  16

#define INVALID_SEGMENT_INDEX                   0xffff

void main(void) {
    // Load the brush instance from vertex attributes.
    Instance instance = decode_instance_attributes();
    int edge_flags = (instance.flags >> 16) & 0xff;
    int brush_flags = (instance.flags >> 24) & 0xff;
    PrimitiveHeader ph = fetch_prim_header(instance.prim_header_address);

    // Fetch the segment of this brush primitive we are drawing.
    vec4 segment_data;
    RectWithSize segment_rect;
    if (instance.segment_index == INVALID_SEGMENT_INDEX) {
        segment_rect = ph.local_rect;
        segment_data = vec4(0.0);
    } else {
        int segment_address = ph.specific_prim_address +
                              VECS_PER_SPECIFIC_BRUSH +
                              instance.segment_index * VECS_PER_SEGMENT;

        vec4[2] segment_info = fetch_from_gpu_cache_2(segment_address);
        segment_rect = RectWithSize(segment_info[0].xy, segment_info[0].zw);
        segment_rect.p0 += ph.local_rect.p0;
        segment_data = segment_info[1];
    }

    VertexInfo vi;

    // Fetch the dynamic picture that we are drawing on.
    PictureTask pic_task = fetch_picture_task(instance.picture_task_address);
    ClipArea clip_area = fetch_clip_area(instance.clip_address);

    Transform transform = fetch_transform(ph.transform_id);

    // Write the normal vertex information out.
    if (transform.is_axis_aligned) {
        vi = write_vertex(
            segment_rect,
            ph.local_clip_rect,
            ph.z,
            transform,
            pic_task
        );

        // TODO(gw): transform bounds may be referenced by
        //           the fragment shader when running in
        //           the alpha pass, even on non-transformed
        //           items. For now, just ensure it has no
        //           effect. We can tidy this up as we move
        //           more items to be brush shaders.
#ifdef WR_FEATURE_ALPHA_PASS
        init_transform_vs(vec4(vec2(-1.0e16), vec2(1.0e16)));
#endif
    } else {
        bvec4 edge_mask = notEqual(edge_flags & ivec4(1, 2, 4, 8), ivec4(0));

        vi = write_transform_vertex(
            segment_rect,
            ph.local_rect,
            ph.local_clip_rect,
            mix(vec4(0.0), vec4(1.0), edge_mask),
            ph.z,
            transform,
            pic_task
        );
    }

    // For brush instances in the alpha pass, always write
    // out clip information.
    // TODO(gw): It's possible that we might want alpha
    //           shaders that don't clip in the future,
    //           but it's reasonable to assume that one
    //           implies the other, for now.
#ifdef WR_FEATURE_ALPHA_PASS
    write_clip(
        vi.world_pos,
        vi.snap_offset,
        clip_area
    );
#endif

    // Run the specific brush VS code to write interpolators.
    brush_vs(
        vi,
        ph.specific_prim_address,
        ph.local_rect,
        segment_rect,
        ph.user_data,
        instance.user_data,
        transform.m,
        pic_task,
        brush_flags,
        segment_data
    );
}
#endif

#ifdef WR_FRAGMENT_SHADER

struct Fragment {
    vec4 color;
#ifdef WR_FEATURE_DUAL_SOURCE_BLENDING
    vec4 blend;
#endif
};

Fragment brush_fs();

void main(void) {
#ifdef WR_FEATURE_DEBUG_OVERDRAW
    oFragColor = WR_DEBUG_OVERDRAW_COLOR;
#else
    // Run the specific brush FS code to output the color.
    Fragment frag = brush_fs();

#ifdef WR_FEATURE_ALPHA_PASS
    // Apply the clip mask
    float clip_alpha = do_clip();

    frag.color *= clip_alpha;

    #ifdef WR_FEATURE_DUAL_SOURCE_BLENDING
        oFragBlend = frag.blend * clip_alpha;
    #endif
#endif

    write_output(frag.color);
#endif
}
#endif
