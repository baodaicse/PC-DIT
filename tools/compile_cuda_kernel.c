/* Build-time NVRTC wrapper. Runtime binaries embed PTX and need only libcuda. */
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
typedef void *Program;
#define LOAD(name, type) type name = (type)dlsym(lib, #name); if (!name) { fprintf(stderr,"missing %s\n",#name); return 2; }
typedef int (*Create)(Program*,const char*,const char*,int,const char*const*,const char*const*);
typedef int (*Compile)(Program,int,const char*const*);
typedef int (*Size)(Program,size_t*);
typedef int (*Get)(Program,char*);
typedef int (*Destroy)(Program*);
int main(int argc, char **argv) {
    if(argc!=4 && argc!=5) { fprintf(stderr,"usage: compiler libnvrtc.so source.cu output.ptx [compute_NN]\n"); return 2; }
    char arch[64]; snprintf(arch,sizeof arch,"--gpu-architecture=%s",argc==5?argv[4]:"compute_52");
    void *lib=dlopen(argv[1],RTLD_NOW|RTLD_LOCAL);
    if(!lib) { fprintf(stderr,"%s\n",dlerror()); return 2; }
    LOAD(nvrtcCreateProgram, Create); LOAD(nvrtcCompileProgram, Compile);
    LOAD(nvrtcGetProgramLogSize, Size); LOAD(nvrtcGetProgramLog, Get);
    LOAD(nvrtcGetPTXSize, Size); LOAD(nvrtcGetPTX, Get); LOAD(nvrtcDestroyProgram, Destroy);
    FILE *f=fopen(argv[2],"rb"); if(!f) return 2;
    if(fseek(f,0,SEEK_END)) return 2; long len=ftell(f); if(len<0) return 2;
    rewind(f); char *source=calloc((size_t)len+1,1); if(!source) return 2;
    if(fread(source,1,(size_t)len,f)!=(size_t)len) return 2; fclose(f);
    Program program=NULL;
    if(nvrtcCreateProgram(&program,source,argv[2],0,NULL,NULL)) return 2;
    const char *options[]={arch,"--std=c++11","--fmad=false"};
    int status=nvrtcCompileProgram(program,3,options);
    size_t size=0; nvrtcGetProgramLogSize(program,&size);
    char *log=calloc(size+1,1); if(!log) return 2;
    nvrtcGetProgramLog(program,log); fprintf(stderr,"%s",log); free(log);
    if(status) { nvrtcDestroyProgram(&program); return 1; }
    if(nvrtcGetPTXSize(program,&size)) return 2;
    char *ptx=malloc(size); if(!ptx || nvrtcGetPTX(program,ptx)) return 2;
    f=fopen(argv[3],"wb"); if(!f) return 2;
    if(fwrite(ptx,1,size,f)!=size || fclose(f)) return 2;
    free(ptx); free(source); nvrtcDestroyProgram(&program); dlclose(lib);
    return 0;
}
