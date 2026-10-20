/* Real stock EGL/GLES rendering through Zink, with no display or compositor. */
#include <EGL/egl.h>
#include <GLES2/gl2.h>
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
static GLuint shader(GLenum kind,const char *source) {
    GLuint result=glCreateShader(kind);assert(result);
    glShaderSource(result,1,&source,NULL);glCompileShader(result);
    GLint ok=0;glGetShaderiv(result,GL_COMPILE_STATUS,&ok);
    if(!ok){char log[2048];glGetShaderInfoLog(result,sizeof(log),NULL,log);fprintf(stderr,"%s\n",log);}
    assert(ok);return result;
}
int main(void) {
    EGLDisplay display=eglGetDisplay(EGL_DEFAULT_DISPLAY);assert(display!=EGL_NO_DISPLAY);
    EGLint major,minor;assert(eglInitialize(display,&major,&minor));
    assert(eglBindAPI(EGL_OPENGL_ES_API));
    EGLint attributes[]={EGL_SURFACE_TYPE,EGL_PBUFFER_BIT,EGL_RENDERABLE_TYPE,EGL_OPENGL_ES2_BIT,
        EGL_RED_SIZE,8,EGL_GREEN_SIZE,8,EGL_BLUE_SIZE,8,EGL_ALPHA_SIZE,8,EGL_NONE};
    EGLConfig config;EGLint count;assert(eglChooseConfig(display,attributes,&config,1,&count)&&count==1);
    EGLint context_attributes[]={EGL_CONTEXT_CLIENT_VERSION,2,EGL_NONE};
    EGLContext context=eglCreateContext(display,config,EGL_NO_CONTEXT,context_attributes);assert(context!=EGL_NO_CONTEXT);
    EGLint surface_attributes[]={EGL_WIDTH,16,EGL_HEIGHT,16,EGL_NONE};
    EGLSurface surface=eglCreatePbufferSurface(display,config,surface_attributes);assert(surface!=EGL_NO_SURFACE);
    assert(eglMakeCurrent(display,surface,surface,context));
    const char *renderer=(const char *)glGetString(GL_RENDERER);assert(renderer);
    printf("EGL %d.%d, renderer %s\n",major,minor,renderer);
    assert(strstr(renderer,"zink") && strstr(renderer,"Apple"));
    GLuint vertex=shader(GL_VERTEX_SHADER,"attribute vec2 position; void main(){gl_Position=vec4(position,0.0,1.0);}");
    GLuint fragment=shader(GL_FRAGMENT_SHADER,"precision mediump float; void main(){gl_FragColor=vec4(0.25,0.5,0.75,1.0);}");
    GLuint program=glCreateProgram();assert(program);glAttachShader(program,vertex);glAttachShader(program,fragment);
    glBindAttribLocation(program,0,"position");glLinkProgram(program);
    GLint linked;glGetProgramiv(program,GL_LINK_STATUS,&linked);assert(linked);glUseProgram(program);
    const GLfloat vertices[]={-1,-1,3,-1,-1,3};
    glViewport(0,0,16,16);glVertexAttribPointer(0,2,GL_FLOAT,GL_FALSE,0,vertices);glEnableVertexAttribArray(0);
    glDrawArrays(GL_TRIANGLES,0,3);glFinish();assert(glGetError()==GL_NO_ERROR);
    unsigned char pixels[16*16*4];glReadPixels(0,0,16,16,GL_RGBA,GL_UNSIGNED_BYTE,pixels);
    assert(glGetError()==GL_NO_ERROR);
    const int expected[]={64,128,191,255};
    for(unsigned pixel=0;pixel<256;pixel++)for(unsigned channel=0;channel<4;channel++)
        assert(abs((int)pixels[pixel*4+channel]-expected[channel])<=2);
    glDeleteProgram(program);glDeleteShader(vertex);glDeleteShader(fragment);
    assert(eglMakeCurrent(display,EGL_NO_SURFACE,EGL_NO_SURFACE,EGL_NO_CONTEXT));
    assert(eglDestroySurface(display,surface));assert(eglDestroyContext(display,context));assert(eglTerminate(display));
    puts("stock Zink surfaceless GLES triangle and all 256 pixels passed");
}
