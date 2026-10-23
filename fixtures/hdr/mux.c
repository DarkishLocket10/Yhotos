/* Synthetic gain-map fixture packaging using the reference API. */
#include <ultrahdr_api.h>
#include <stdio.h>
#include <stdlib.h>
static uhdr_compressed_image_t load(const char *name) {
    FILE *f = fopen(name, "rb");
    if (!f || fseek(f, 0, SEEK_END)) exit(1);
    long n = ftell(f);
    if (n <= 0 || n > 268435456 || fseek(f,0,SEEK_SET)) exit(1);
    void *p = malloc((size_t)n);
    if (!p || fread(p,1,(size_t)n,f) != (size_t)n || fclose(f)) exit(1);
    return (uhdr_compressed_image_t){p,(size_t)n,(size_t)n,UHDR_CG_UNSPECIFIED,UHDR_CT_UNSPECIFIED,UHDR_CR_UNSPECIFIED};
}
int main(int argc,char **argv) {
    if(argc!=4) return 1;
    uhdr_compressed_image_t base=load(argv[1]), gain=load(argv[2]);
    uhdr_gainmap_metadata_t m = {{4,4,4},{1,1,1},{1,1,1},{0,0,0},{0,0,0},1,4,1};
    uhdr_codec_private_t *enc=uhdr_create_encoder();
    if(!enc || uhdr_enc_set_compressed_image(enc,&base,UHDR_BASE_IMG).error_code || uhdr_enc_set_gainmap_image(enc,&gain,&m).error_code || uhdr_encode(enc).error_code) return 1;
    const uhdr_compressed_image_t *out=uhdr_get_encoded_stream(enc);
    FILE *f=fopen(argv[3],"wb");
    if(!f || !out || fwrite(out->data,1,out->data_sz,f)!=out->data_sz || fclose(f)) return 1;
    uhdr_release_encoder(enc);free(base.data);free(gain.data);return 0;
}
