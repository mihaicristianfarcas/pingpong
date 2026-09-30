// BGRA -> NV12 with BT.709 limited-range coefficients, which is what H.264
// consumers expect. Reads through a CUDA texture object bound to the CUarray
// obtained from the registered D3D11 texture, so the swizzled/tiled layout is
// handled by the texture unit rather than by us.
//
// NVRTC does not implicitly define cudaTextureObject_t, so declare it. It is
// just an opaque 64-bit handle.
typedef unsigned long long cudaTextureObject_t;

extern "C" __global__ void bgra_to_nv12(
    cudaTextureObject_t src,
    unsigned char* __restrict__ y_plane,
    unsigned char* __restrict__ uv_plane,
    int width, int height, int y_pitch, int uv_pitch)
{
    int x = (blockIdx.x * blockDim.x + threadIdx.x) * 2;
    int y = (blockIdx.y * blockDim.y + threadIdx.y) * 2;
    if (x >= width || y >= height) return;

    float4 p[4];
    p[0] = tex2D<float4>(src, x + 0.5f, y + 0.5f);
    p[1] = tex2D<float4>(src, x + 1.5f, y + 0.5f);
    p[2] = tex2D<float4>(src, x + 0.5f, y + 1.5f);
    p[3] = tex2D<float4>(src, x + 1.5f, y + 1.5f);

    float u_sum = 0.0f, v_sum = 0.0f;
    for (int i = 0; i < 4; ++i) {
        // tex2D on a BGRA8 texture yields .x=B .y=G .z=R
        float b = p[i].x, g = p[i].y, r = p[i].z;
        float yy =  0.1826f*r + 0.6142f*g + 0.0620f*b + 0.0625f;
        float uu = -0.1006f*r - 0.3386f*g + 0.4392f*b + 0.5000f;
        float vv =  0.4392f*r - 0.3989f*g - 0.0403f*b + 0.5000f;

        int px = x + (i & 1), py = y + (i >> 1);
        if (px < width && py < height) {
            y_plane[py * y_pitch + px] =
                (unsigned char)fminf(fmaxf(yy * 255.0f, 0.0f), 255.0f);
        }
        u_sum += uu; v_sum += vv;
    }

    int cx = x >> 1, cy = y >> 1;
    uv_plane[cy * uv_pitch + cx * 2 + 0] =
        (unsigned char)fminf(fmaxf((u_sum * 0.25f) * 255.0f, 0.0f), 255.0f);
    uv_plane[cy * uv_pitch + cx * 2 + 1] =
        (unsigned char)fminf(fmaxf((v_sum * 0.25f) * 255.0f, 0.0f), 255.0f);
}
